//! User hooks around tool calls (`~/.cacode/hooks.toml`, plus
//! `<root>/.cacode/hooks.toml` in trusted workspaces):
//!
//! ```toml
//! [[pre_tool]]
//! matcher = "bash"                  # regex on the tool name
//! command = "scripts/guard.sh"      # receives JSON on stdin
//!
//! [[post_tool]]
//! matcher = "edit_file|write_file"
//! command = "cargo fmt --all"
//! timeout_ms = 60000
//! ```
//! Exit code 0 = continue (stdout of post hooks is shown to the model),
//! exit code 2 = block (pre) / report failure (post) with stderr as the
//! reason, any other code = warning only.

use anyhow::Result;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone, Deserialize)]
struct HookCfg {
    #[serde(default = "any")]
    matcher: String,
    command: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}
fn any() -> String {
    ".*".into()
}
fn default_timeout() -> u64 {
    60_000
}

#[derive(Debug, Default, Deserialize)]
struct HooksFile {
    #[serde(default)]
    pre_tool: Vec<HookCfg>,
    #[serde(default)]
    post_tool: Vec<HookCfg>,
}

#[derive(Debug, Clone)]
pub struct Hook {
    matcher: Regex,
    pub command: String,
    timeout: Duration,
}

#[derive(Debug, Clone, Default)]
pub struct HookSet {
    pub pre: Vec<Hook>,
    pub post: Vec<Hook>,
    pub errors: Vec<String>,
}

pub enum PreOutcome {
    Continue(Vec<String>),
    Block(String),
}

impl HookSet {
    pub fn load(root: &Path, trusted: bool) -> Self {
        let mut files = Vec::new();
        if let Some(h) = dirs::home_dir() {
            files.push(h.join(".cacode").join("hooks.toml"));
        }
        if trusted {
            files.push(root.join(".cacode").join("hooks.toml"));
        }
        let mut set = HookSet::default();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            match toml::from_str::<HooksFile>(&text) {
                Ok(h) => {
                    for (list, out) in [(h.pre_tool, &mut set.pre), (h.post_tool, &mut set.post)] {
                        for c in list {
                            match Regex::new(&format!("^(?:{})$", c.matcher)) {
                                Ok(re) => out.push(Hook { matcher: re, command: c.command, timeout: Duration::from_millis(c.timeout_ms.clamp(1_000, 600_000)) }),
                                Err(e) => set.errors.push(format!("{}: bad matcher '{}': {}", f.display(), c.matcher, e)),
                            }
                        }
                    }
                }
                Err(e) => set.errors.push(format!("{}: {}", f.display(), e)),
            }
        }
        set
    }

    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty()
    }

    async fn run(h: &Hook, root: &Path, payload: &serde_json::Value, tool: &str) -> (Option<i32>, String, String) {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(&h.command).current_dir(root).env("CACODE_TOOL", tool).env("CACODE_HOOK", "1");
        cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        let Ok(mut child) = cmd.spawn() else { return (None, String::new(), format!("cannot spawn hook `{}`", h.command)) };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        }
        match tokio::time::timeout(h.timeout, child.wait_with_output()).await {
            Ok(Ok(o)) => (o.status.code(), String::from_utf8_lossy(&o.stdout).trim().to_string(), String::from_utf8_lossy(&o.stderr).trim().to_string()),
            Ok(Err(e)) => (None, String::new(), e.to_string()),
            Err(_) => (None, String::new(), format!("hook `{}` timed out", h.command)),
        }
    }

    pub async fn pre_tool(&self, root: &Path, tool: &str, args: &serde_json::Value) -> PreOutcome {
        let mut notes = Vec::new();
        for h in self.pre.iter().filter(|h| h.matcher.is_match(tool)) {
            let payload = json!({"event": "pre_tool", "tool": tool, "args": args, "workspace": root});
            let (code, out, err) = Self::run(h, root, &payload, tool).await;
            match code {
                Some(0) => {
                    if !out.is_empty() {
                        notes.push(out);
                    }
                }
                Some(2) => return PreOutcome::Block(format!("Blocked by pre_tool hook `{}`: {}", h.command, if err.is_empty() { out } else { err })),
                _ => notes.push(format!("[hook warning] `{}` exited {:?}: {}", h.command, code, err)),
            }
        }
        PreOutcome::Continue(notes)
    }

    /// Returns (notes to append, failure message if a hook exited 2).
    pub async fn post_tool(&self, root: &Path, tool: &str, args: &serde_json::Value, output: &str, is_error: bool) -> (Vec<String>, Option<String>) {
        let mut notes = Vec::new();
        let mut fail = None;
        for h in self.post.iter().filter(|h| h.matcher.is_match(tool)) {
            let payload = json!({"event": "post_tool", "tool": tool, "args": args, "output": crate::text::clamp_output(output, 20_000, 300), "is_error": is_error, "workspace": root});
            let (code, out, err) = Self::run(h, root, &payload, tool).await;
            match code {
                Some(0) => {
                    if !out.is_empty() {
                        notes.push(format!("[post_tool hook `{}`]\n{}", h.command, crate::text::clamp_output(&out, 4_000, 80)));
                    }
                }
                Some(2) => fail = Some(format!("post_tool hook `{}` failed: {}", h.command, crate::text::clamp_output(if err.is_empty() { &out } else { &err }, 4_000, 80))),
                _ => notes.push(format!("[hook warning] `{}` exited {:?}: {}", h.command, code, err)),
            }
        }
        (notes, fail)
    }
}

pub fn parse_for_test(text: &str) -> Result<HookSet> {
    let h: HooksFile = toml::from_str(text)?;
    let mut set = HookSet::default();
    for c in h.pre_tool {
        set.pre.push(Hook { matcher: Regex::new(&format!("^(?:{})$", c.matcher))?, command: c.command, timeout: Duration::from_millis(c.timeout_ms) });
    }
    for c in h.post_tool {
        set.post.push(Hook { matcher: Regex::new(&format!("^(?:{})$", c.matcher))?, command: c.command, timeout: Duration::from_millis(c.timeout_ms) });
    }
    Ok(set)
}
