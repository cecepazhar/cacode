//! Shell runner: foreground with timeout + exit-code capture, background jobs,
//! process-group kill on timeout / cancel, API-key scrubbing.

use super::{Approval, ToolContext};
use crate::events::ApprovalRequest;
use crate::permission::{is_allowlisted, ActionKind};
use crate::text::{clamp_default, clamp_output};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Hard cap on bytes buffered per stream (the observation is clamped further).
const CAPTURE_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ExecRecord {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_excerpt: String,
}

#[derive(Default)]
struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    dropped: usize,
}

impl Captured {
    fn push(&mut self, err: bool, chunk: &[u8]) {
        let buf = if err { &mut self.stderr } else { &mut self.stdout };
        let room = CAPTURE_CAP.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        self.dropped += chunk.len().saturating_sub(room);
    }
}

struct Job {
    command: String,
    started: Instant,
    pgid: Option<i32>,
    out: Arc<Mutex<Captured>>,
    read_pos: (usize, usize),
    status: Arc<Mutex<Option<String>>>,
}

/// Registry of background shells, shared by a session.
#[derive(Default)]
pub struct JobRegistry {
    next: AtomicU64,
    jobs: Mutex<HashMap<String, Job>>,
}

impl std::fmt::Debug for JobRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JobRegistry({} jobs)", self.jobs.lock().map(|j| j.len()).unwrap_or(0))
    }
}

impl JobRegistry {
    pub fn list(&self) -> Vec<(String, String, bool)> {
        self.jobs
            .lock()
            .map(|j| j.iter().map(|(id, job)| (id.clone(), job.command.clone(), job.status.lock().map(|s| s.is_some()).unwrap_or(true))).collect())
            .unwrap_or_default()
    }

    pub fn kill_all(&self) {
        if let Ok(jobs) = self.jobs.lock() {
            for job in jobs.values() {
                if let Some(pg) = job.pgid {
                    kill_group(pg);
                }
            }
        }
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[cfg(unix)]
fn kill_group(pgid: i32) {
    // SAFETY: plain syscall; negative pid targets the whole process group.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}
#[cfg(not(unix))]
fn kill_group(_pgid: i32) {}

/// Kills the process group when dropped unless disarmed (covers cancel).
struct GroupGuard(Option<i32>);
impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pg) = self.0 {
            kill_group(pg);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Never wrap commands.
    Off,
    /// Use bubblewrap when it is installed and works; otherwise run unwrapped.
    Auto,
    /// Refuse to run commands when bubblewrap is unavailable.
    Required,
}

/// OS-level sandbox for `bash` (Linux bubblewrap): the whole filesystem is
/// read-only except the workspace, a private /tmp and the listed caches.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub writable: Vec<std::path::PathBuf>,
    pub network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_default();
        Self {
            mode: SandboxMode::Auto,
            writable: [".cargo/registry", ".cargo/git", ".cache", "go/pkg/mod", ".npm", ".rustup/tmp", ".gradle/caches"].iter().map(|p| home.join(p)).collect(),
            network: true,
        }
    }
}

impl SandboxConfig {
    pub fn from_settings(mode: &str, extra_writable: &[String], network: bool) -> Self {
        let mode = match std::env::var("CACODE_SANDBOX").unwrap_or_else(|_| mode.to_string()).to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "none" => SandboxMode::Off,
            "required" | "on" | "strict" => SandboxMode::Required,
            _ => SandboxMode::Auto,
        };
        let mut c = Self { mode, ..Self::default() };
        let home = dirs::home_dir().unwrap_or_default();
        for w in extra_writable {
            c.writable.push(match w.strip_prefix("~/") {
                Some(r) => home.join(r),
                None => std::path::PathBuf::from(w),
            });
        }
        c.network = network;
        c
    }
}

/// bubblewrap installed and able to create a sandbox here (checked once).
pub fn bwrap_available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "--tmpfs", "/tmp", "true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Whether this context will wrap commands (`Err` = required but unavailable).
pub fn sandbox_active(ctx: &ToolContext) -> Result<bool> {
    match ctx.sandbox.mode {
        SandboxMode::Off => Ok(false),
        SandboxMode::Auto => Ok(cfg!(target_os = "linux") && bwrap_available()),
        SandboxMode::Required => {
            if cfg!(target_os = "linux") && bwrap_available() {
                Ok(true)
            } else {
                Err(anyhow!("sandbox=required but bubblewrap (bwrap) is not available; install it or set CACODE_SANDBOX=auto"))
            }
        }
    }
}

const SECRET_ENV_HINTS: &[&str] = &["API_KEY", "_TOKEN", "SECRET", "PASSWORD", "LICENSE"];

fn build_command(ctx: &ToolContext, cmd: &str, sandboxed: bool) -> tokio::process::Command {
    let mut c = if sandboxed {
        let root = ctx.workspace.root();
        let mut c = tokio::process::Command::new("bwrap");
        c.args(["--die-with-parent", "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "--tmpfs", "/tmp"]);
        for w in &ctx.sandbox.writable {
            if w.exists() && !root.starts_with(w) {
                c.arg("--bind").arg(w).arg(w);
            }
        }
        c.arg("--bind").arg(root).arg(root);
        if !ctx.sandbox.network {
            c.arg("--unshare-net");
        }
        c.arg("--chdir").arg(root).args(["--", "bash", "-c", cmd]);
        c
    } else {
        let mut c = tokio::process::Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    };
    c.current_dir(ctx.workspace.root());
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c.kill_on_drop(true);
    for (k, _) in std::env::vars() {
        if SECRET_ENV_HINTS.iter().any(|h| k.to_ascii_uppercase().contains(h)) {
            c.env_remove(&k);
        }
    }
    c.env("CACODE_AGENT", "1");
    c.env("GIT_PAGER", "cat").env("PAGER", "cat");
    #[cfg(unix)]
    c.process_group(0);
    c
}

fn spawn_readers(child: &mut tokio::process::Child, out: Arc<Mutex<Captured>>) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    if let Some(mut so) = child.stdout.take() {
        let o = out.clone();
        handles.push(tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            while let Ok(n) = so.read(&mut buf).await {
                if n == 0 { break; }
                if let Ok(mut g) = o.lock() { g.push(false, &buf[..n]); }
            }
        }));
    }
    if let Some(mut se) = child.stderr.take() {
        let o = out;
        handles.push(tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            while let Ok(n) = se.read(&mut buf).await {
                if n == 0 { break; }
                if let Ok(mut g) = o.lock() { g.push(true, &buf[..n]); }
            }
        }));
    }
    handles
}

/// Tokens that reference paths outside the workspace make a command non-allowlisted.
fn references_outside(ctx: &ToolContext, cmd: &str) -> bool {
    cmd.split_whitespace().any(|tok| {
        let t = tok.trim_matches(|c| c == '"' || c == '\'');
        let looks_path = t.starts_with('/') || t.starts_with('~') || t.contains("..");
        looks_path && ctx.workspace.resolve(t, crate::tools::workspace::Access::Read).is_err()
    })
}

fn format_result(cmd: &str, code: Option<i32>, timed_out: bool, elapsed: Duration, cap: &Captured) -> String {
    let stdout = String::from_utf8_lossy(&cap.stdout);
    let stderr = String::from_utf8_lossy(&cap.stderr);
    let status = if timed_out {
        format!("killed: timeout after {:.1}s", elapsed.as_secs_f64())
    } else {
        match code {
            Some(c) => format!("exit_code: {}", c),
            None => "exit_code: none (terminated by signal)".into(),
        }
    };
    let mut s = format!("$ {}\n{} ({:.1}s)\n", cmd, status, elapsed.as_secs_f64());
    if !stdout.is_empty() {
        s.push_str(&format!("--- stdout ---\n{}\n", clamp_output(&stdout, 24_000, 400)));
    }
    if !stderr.is_empty() {
        s.push_str(&format!("--- stderr ---\n{}\n", clamp_output(&stderr, 8_000, 150)));
    }
    if cap.dropped > 0 {
        s.push_str(&format!("[{} bytes beyond capture cap discarded]\n", cap.dropped));
    }
    if stdout.is_empty() && stderr.is_empty() {
        s.push_str("(no output)\n");
    }
    s
}

pub async fn bash(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let cmd = args.get("command").or_else(|| args.get("cmd")).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing command"))?;
    let background = args.get("run_in_background").and_then(|v| v.as_bool()).unwrap_or(false);
    let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(ctx.bash_default_timeout_ms).clamp(1_000, MAX_TIMEOUT_MS);

    let allow = is_allowlisted(cmd, &ctx.bash_allowlist) && !references_outside(ctx, cmd) && !background;
    match ctx
        .authorize(ActionKind::Exec, allow, ApprovalRequest { id: 0, tool: "bash".into(), summary: format!("Run: {}", cmd), diff: None })
        .await?
    {
        Approval::Granted => {}
        Approval::Denied(reason) => return Err(anyhow!("Rejected command `{}` — {}", cmd, reason)),
    }

    let sandboxed = sandbox_active(ctx)?;
    let mut command = build_command(ctx, cmd, sandboxed);
    let mut child = command.spawn().map_err(|e| anyhow!("Failed to spawn bash: {}", e))?;
    let pgid = child.id().map(|p| p as i32);
    let out = Arc::new(Mutex::new(Captured::default()));
    let readers = spawn_readers(&mut child, out.clone());
    let started = Instant::now();

    if background {
        let status: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let st = status.clone();
        tokio::spawn(async move {
            let res = child.wait().await;
            for r in readers { let _ = r.await; }
            if let Ok(mut s) = st.lock() {
                *s = Some(match res {
                    Ok(es) => format!("exit_code: {}", es.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())),
                    Err(e) => format!("error: {}", e),
                });
            }
        });
        let id = format!("bash_{}", ctx.jobs.next.fetch_add(1, Ordering::SeqCst) + 1);
        ctx.jobs.jobs.lock().map_err(|_| anyhow!("job registry poisoned"))?.insert(
            id.clone(),
            Job { command: cmd.to_string(), started, pgid, out, read_pos: (0, 0), status },
        );
        return Ok(format!("Started background job {} for `{}`. Use bash_output with job_id=\"{}\" to read output, kill_shell to stop it.", id, cmd, id));
    }

    let mut guard = GroupGuard(pgid);
    let waited = tokio::select! {
        r = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()) => r,
        _ = ctx.cancel.cancelled() => {
            // guard kills the whole group on drop
            return Err(anyhow!("Cancelled by user: `{}` was killed", cmd));
        }
    };
    let (code, timed_out) = match waited {
        Ok(Ok(status)) => {
            guard.0 = None;
            (status.code(), false)
        }
        Ok(Err(e)) => return Err(anyhow!("wait failed: {}", e)),
        Err(_) => {
            if let Some(pg) = pgid { kill_group(pg); }
            let _ = child.kill().await;
            (None, true)
        }
    };
    for r in readers {
        let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
    }
    let cap = std::mem::take(&mut *out.lock().map_err(|_| anyhow!("capture poisoned"))?);
    let mut result = format_result(cmd, code, timed_out, started.elapsed(), &cap);
    if sandboxed && code != Some(0) && String::from_utf8_lossy(&cap.stderr).contains("Read-only file system") {
        result.push_str("[sandbox] the command tried to write outside the workspace; only the workspace, a private /tmp and package caches are writable (host /tmp is not visible). Ask the user to run it themselves or to set CACODE_SANDBOX=off.\n");
    }
    ctx.log_exec(ExecRecord {
        command: cmd.to_string(),
        exit_code: code,
        timed_out,
        output_excerpt: clamp_output(&result, 1_500, 30),
    });
    Ok(result)
}

pub fn bash_output(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let id = args.get("job_id").or_else(|| args.get("bash_id")).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing job_id"))?;
    let mut jobs = ctx.jobs.jobs.lock().map_err(|_| anyhow!("job registry poisoned"))?;
    let job = jobs.get_mut(id).ok_or_else(|| anyhow!("Unknown job_id {}", id))?;
    let (new_out, new_err) = {
        let cap = job.out.lock().map_err(|_| anyhow!("capture poisoned"))?;
        let o = String::from_utf8_lossy(&cap.stdout[job.read_pos.0.min(cap.stdout.len())..]).to_string();
        let e = String::from_utf8_lossy(&cap.stderr[job.read_pos.1.min(cap.stderr.len())..]).to_string();
        job.read_pos = (cap.stdout.len(), cap.stderr.len());
        (o, e)
    };
    let status = job.status.lock().ok().and_then(|s| s.clone()).unwrap_or_else(|| format!("running ({:.0}s)", job.started.elapsed().as_secs_f64()));
    Ok(format!(
        "job {} `{}` — {}\n--- new stdout ---\n{}\n--- new stderr ---\n{}",
        id,
        job.command,
        status,
        clamp_default(&new_out),
        clamp_output(&new_err, 8_000, 150)
    ))
}

pub fn kill_shell(ctx: &ToolContext, args: &serde_json::Value) -> Result<String> {
    let id = args.get("job_id").or_else(|| args.get("shell_id")).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing job_id"))?;
    let jobs = ctx.jobs.jobs.lock().map_err(|_| anyhow!("job registry poisoned"))?;
    let job = jobs.get(id).ok_or_else(|| anyhow!("Unknown job_id {}", id))?;
    if let Some(pg) = job.pgid {
        kill_group(pg);
    }
    Ok(format!("Sent SIGKILL to job {} (`{}`)", id, job.command))
}
