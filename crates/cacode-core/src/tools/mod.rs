//! Tool layer: specs (JSON Schema, sent as native tool definitions), the
//! per-session [`ToolContext`] and the dispatcher.

pub mod bash;
pub mod fs;
pub mod planning;
pub mod search;
pub mod web;
pub mod workspace;

use crate::events::{AgentEvent, AgentEvents, ApprovalDecision, ApprovalRequest, ApprovalTicket};
use crate::permission::{evaluate, ActionKind, PermissionMode, Verdict};
use crate::symbols::SymbolIndex;
use crate::text::clamp_default;
use crate::types::ToolCall;
use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub use bash::{ExecRecord, JobRegistry};
pub use fs::{FileStamp, UndoStore};
pub use workspace::{Access, Workspace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Granted,
    Denied(String),
}

/// Spawns subagents for the `task` tool (implemented in `subagent.rs`).
pub trait SubagentSpawner: Send + Sync {
    fn spawn(&self, args: serde_json::Value, parent: ToolContext) -> BoxFuture<'static, Result<String>>;
}

/// State shared by every tool call in one session (cheap to clone).
#[derive(Clone)]
pub struct ToolContext {
    pub workspace: Workspace,
    pub mode: PermissionMode,
    pub bash_allowlist: Vec<String>,
    pub bash_default_timeout_ms: u64,
    pub sandbox: bash::SandboxConfig,
    pub web_allowlist: Vec<String>,
    pub file_state: Arc<Mutex<HashMap<PathBuf, FileStamp>>>,
    pub undo: Arc<UndoStore>,
    pub jobs: Arc<JobRegistry>,
    pub events: Option<AgentEvents>,
    pub cancel: CancellationToken,
    pub always_edit: Arc<AtomicBool>,
    pub approved_cmds: Arc<Mutex<HashSet<String>>>,
    /// Paths currently being written (prevents parallel subagents clobbering a file).
    pub write_locks: Arc<Mutex<HashSet<PathBuf>>>,
    /// Ground-truth log of executed commands (used as subagent evidence).
    pub exec_log: Arc<Mutex<Vec<ExecRecord>>>,
    pub files_touched: Arc<Mutex<BTreeSet<String>>>,
    pub symbols: Arc<Mutex<Option<SymbolIndex>>>,
    pub subagents: Option<Arc<dyn SubagentSpawner>>,
    pub mcp: Option<Arc<crate::mcp::McpManager>>,
    pub hooks: Arc<crate::hooks::HookSet>,
    /// Workspace is trusted (project hooks / MCP allowed).
    pub trusted: bool,
    /// Agent-managed todo list (`todo_write`).
    pub todos: Arc<Mutex<Vec<crate::types::TodoEntry>>>,
    /// Shared permission mode cell (plan mode can switch it mid-turn).
    mode_cell: Arc<Mutex<PermissionMode>>,
    next_approval: Arc<AtomicU64>,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext").field("root", &self.workspace.root()).field("mode", &self.mode).finish()
    }
}

/// RAII lock on a path being written.
#[derive(Debug)]
pub struct PathLock {
    set: Arc<Mutex<HashSet<PathBuf>>>,
    path: PathBuf,
}
impl Drop for PathLock {
    fn drop(&mut self) {
        if let Ok(mut s) = self.set.lock() {
            s.remove(&self.path);
        }
    }
}

impl ToolContext {
    pub fn new(workspace: Workspace, mode: PermissionMode) -> Self {
        let undo = Arc::new(UndoStore::for_workspace(workspace.root()));
        Self {
            workspace,
            mode,
            bash_allowlist: crate::permission::default_allowlist(),
            bash_default_timeout_ms: bash::DEFAULT_TIMEOUT_MS,
            sandbox: bash::SandboxConfig::from_settings("auto", &[], true),
            web_allowlist: web::DEFAULT_WEB_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
            file_state: Arc::default(),
            undo,
            jobs: Arc::default(),
            events: None,
            cancel: CancellationToken::new(),
            always_edit: Arc::default(),
            approved_cmds: Arc::default(),
            write_locks: Arc::default(),
            exec_log: Arc::default(),
            files_touched: Arc::default(),
            symbols: Arc::default(),
            subagents: None,
            mcp: None,
            hooks: Arc::default(),
            trusted: false,
            todos: Arc::default(),
            mode_cell: Arc::new(Mutex::new(mode)),
            next_approval: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Current permission mode (may change mid-turn, e.g. plan approved).
    pub fn mode(&self) -> PermissionMode {
        self.mode_cell.lock().map(|m| *m).unwrap_or(self.mode)
    }

    pub fn set_mode(&mut self, m: PermissionMode) {
        self.mode = m;
        if let Ok(mut c) = self.mode_cell.lock() {
            *c = m;
        }
    }

    /// Attach hooks + MCP servers according to workspace trust.
    pub fn with_extensions(mut self) -> Self {
        let root = self.workspace.root().to_path_buf();
        self.trusted = crate::trust::is_trusted(&root);
        self.hooks = Arc::new(crate::hooks::HookSet::load(&root, self.trusted));
        self.mcp = crate::mcp::McpManager::new(&root, crate::mcp::load_configs(&root, self.trusted));
        self
    }

    /// A child context for a subagent: same workspace, locks and undo journal,
    /// its own read-state, logs and permission mode; no further nesting.
    pub fn child(&self, mode: PermissionMode, cancel: CancellationToken) -> Self {
        let mut c = self.clone();
        c.mode = mode;
        c.mode_cell = Arc::new(Mutex::new(mode));
        c.todos = Arc::default();
        c.cancel = cancel;
        c.events = None;
        c.file_state = Arc::default();
        c.exec_log = Arc::default();
        c.files_touched = Arc::default();
        c.subagents = None;
        c.approved_cmds = Arc::new(Mutex::new(self.approved_cmds.lock().map(|s| s.clone()).unwrap_or_default()));
        c
    }

    pub fn record_read(&self, path: &Path, bytes: &[u8]) {
        if let Ok(mut m) = self.file_state.lock() {
            m.insert(path.to_path_buf(), FileStamp { sha256: fs::sha256_hex(bytes) });
        }
    }

    pub fn stamp(&self, path: &Path) -> Option<FileStamp> {
        self.file_state.lock().ok().and_then(|m| m.get(path).cloned())
    }

    pub fn touch(&self, display: &str) {
        if let Ok(mut s) = self.files_touched.lock() {
            s.insert(display.to_string());
        }
    }

    pub fn log_exec(&self, rec: ExecRecord) {
        if let Ok(mut l) = self.exec_log.lock() {
            l.push(rec);
        }
    }

    pub fn lock_path(&self, path: &Path) -> Result<PathLock> {
        let mut s = self.write_locks.lock().map_err(|_| anyhow!("lock poisoned"))?;
        if !s.insert(path.to_path_buf()) {
            return Err(anyhow!("FileLocked: {} is being written by another agent right now; retry later", self.workspace.display(path)));
        }
        Ok(PathLock { set: self.write_locks.clone(), path: path.to_path_buf() })
    }

    /// Permission gate. Asks the front-end when the policy says so.
    pub async fn authorize(&self, kind: ActionKind, allowlisted: bool, mut req: ApprovalRequest) -> Result<Approval> {
        let pre_approved = match kind {
            ActionKind::Exec => self.approved_cmds.lock().map(|s| s.contains(req.summary.trim_start_matches("Run: "))).unwrap_or(false),
            ActionKind::Edit => self.always_edit.load(Ordering::SeqCst),
        };
        let mode = self.mode();
        let verdict = evaluate(mode, kind, allowlisted || (kind == ActionKind::Exec && pre_approved), kind == ActionKind::Edit && pre_approved);
        match verdict {
            Verdict::Allow => Ok(Approval::Granted),
            Verdict::Deny(r) => Ok(Approval::Denied(r)),
            Verdict::Ask => {
                let Some(tx) = &self.events else {
                    return Ok(Approval::Denied(format!(
                        "approval required in '{}' mode but no interactive front-end is attached",
                        mode.label()
                    )));
                };
                req.id = self.next_approval.fetch_add(1, Ordering::SeqCst);
                let (ticket, rx) = ApprovalTicket::new(req.clone());
                tx.send(AgentEvent::ApprovalRequired(ticket)).map_err(|_| anyhow!("front-end disconnected"))?;
                let decision = tokio::select! {
                    d = rx => d.unwrap_or(ApprovalDecision::Reject(Some("approval dialog closed".into()))),
                    _ = self.cancel.cancelled() => return Err(anyhow!("Cancelled by user while waiting for approval")),
                };
                Ok(match decision {
                    ApprovalDecision::Approve => Approval::Granted,
                    ApprovalDecision::ApproveAlways => {
                        match kind {
                            ActionKind::Edit => self.always_edit.store(true, Ordering::SeqCst),
                            ActionKind::Exec => {
                                if let Ok(mut s) = self.approved_cmds.lock() {
                                    s.insert(req.summary.trim_start_matches("Run: ").to_string());
                                }
                            }
                        }
                        Approval::Granted
                    }
                    ApprovalDecision::Reject(reason) => Approval::Denied(reason.unwrap_or_else(|| "rejected by user".into())),
                })
            }
        }
    }

    fn symbols_find(&self, args: &serde_json::Value) -> Result<String> {
        let name = args.get("name").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("Missing name"))?;
        let kind = args.get("kind").and_then(|v| v.as_str());
        let mut guard = self.symbols.lock().map_err(|_| anyhow!("index poisoned"))?;
        match guard.as_mut() {
            Some(idx) => {
                idx.refresh();
            }
            None => *guard = Some(SymbolIndex::load_or_build(self.workspace.root())),
        }
        let idx = guard.as_ref().expect("index set above");
        let hits = idx.find(name, kind, 50);
        if hits.is_empty() {
            return Ok(format!("No symbol matching '{}' ({} symbols indexed). Try grep.", name, idx.len()));
        }
        Ok(hits.iter().map(|s| format!("{}:{}  {} {}{}", s.path, s.line, s.kind, s.name, if s.public { " (pub)" } else { "" })).collect::<Vec<_>>().join("\n"))
    }
}

/// Every tool the model may call. `read_only` tools never need approval.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub read_only: bool,
    pub schema: serde_json::Value,
}

fn spec(name: &str, read_only: bool, description: &str, schema: serde_json::Value) -> ToolSpec {
    ToolSpec { name: name.into(), description: description.into(), read_only, schema }
}

pub fn all_specs() -> Vec<ToolSpec> {
    vec![
        spec("read_file", true, "Read a text file inside the workspace. Returns numbered lines (cat -n style) and a header `lines X-Y of N`. Paginate large files with offset/limit (max 2000 lines per call). You must read a file before editing it.", json!({"type":"object","properties":{"path":{"type":"string","description":"Path relative to the workspace root"},"offset":{"type":"integer","description":"1-based first line","minimum":1},"limit":{"type":"integer","description":"Max lines (default/max 2000)"}},"required":["path"]})),
        spec("write_file", false, "Create a new file or fully overwrite an existing one (existing files must have been read first and unchanged since). Atomic write; the user may be shown a diff for approval.", json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string","description":"Complete new file content"}},"required":["path","content"]})),
        spec("edit_file", false, "Replace an exact snippet in a file. old_string must match exactly one location (copy it from read_file output WITHOUT the line-number prefix); include enough context to be unique, or set replace_all. Fails rather than guessing when the match is ambiguous.", json!({"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean","default":false}},"required":["path","old_string","new_string"]})),
        spec("bash", false, "Run a shell command (bash -c) in the workspace root. Returns exit_code, stdout and stderr (truncated head+tail). Default timeout 120000 ms, max 600000. Set run_in_background for servers/watchers and poll with bash_output. Prefer grep/glob/read_file over cat/find/ls.", json!({"type":"object","properties":{"command":{"type":"string"},"timeout_ms":{"type":"integer"},"run_in_background":{"type":"boolean","default":false}},"required":["command"]})),
        spec("bash_output", true, "Read new output and status of a background bash job.", json!({"type":"object","properties":{"job_id":{"type":"string"}},"required":["job_id"]})),
        spec("kill_shell", false, "Kill a background bash job (whole process group).", json!({"type":"object","properties":{"job_id":{"type":"string"}},"required":["job_id"]})),
        spec("grep", true, "Regex search over file contents (ripgrep-style, respects .gitignore/.cacodeignore, skips binaries and target/node_modules). output_mode: content (path:line:text, default), files_with_matches, count.", json!({"type":"object","properties":{"pattern":{"type":"string","description":"Rust regex"},"path":{"type":"string","description":"File or directory (default .)"},"glob":{"type":"string","description":"e.g. *.rs or src/**/*.ts"},"case_insensitive":{"type":"boolean"},"output_mode":{"type":"string","enum":["content","files_with_matches","count"]},"head_limit":{"type":"integer"}},"required":["pattern"]})),
        spec("glob", true, "Find files by glob pattern (e.g. **/*.rs), newest first, ignore-aware.", json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"limit":{"type":"integer"}},"required":["pattern"]})),
        spec("list_dir", true, "Ignore-aware directory tree.", json!({"type":"object","properties":{"path":{"type":"string"},"depth":{"type":"integer","minimum":1,"maximum":6}}})),
        spec("find_symbol", true, "Locate definitions (fn/struct/enum/trait/class/func/type…) by name across the repo (Rust, Go, TS/JS, Python, C#). Exact matches first, then prefix/substring.", json!({"type":"object","properties":{"name":{"type":"string"},"kind":{"type":"string"}},"required":["name"]})),
        spec("web_fetch", true, "Fetch a URL (docs, API references, raw files) and return it as plain text (HTML converted). Domains outside the allowlist need user approval. Max 5 MB, output truncated.", json!({"type":"object","properties":{"url":{"type":"string","description":"http(s) URL"}},"required":["url"]})),
        spec("todo_write", true, "Replace your task list for this session (shown to the user). Use it for any task with 3+ steps: list steps as pending, mark exactly one in_progress while working, mark completed immediately after finishing.", json!({"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"]}}},"required":["todos"]})),
        spec("exit_plan_mode", true, "Only in plan mode: present your implementation plan (markdown) to the user for approval. On approval the session leaves plan mode and you may edit files.", json!({"type":"object","properties":{"plan":{"type":"string"}},"required":["plan"]})),
        spec("task", true, "Delegate a self-contained sub-task to an independent subagent (fresh context, read-only tools by default). Several task calls in one response run in parallel. Returns a structured report whose evidence comes from commands the subagent actually ran; claims without evidence are flagged unverified.", json!({"type":"object","properties":{"description":{"type":"string","description":"3-6 word label"},"prompt":{"type":"string","description":"Full instructions incl. what to return"},"mode":{"type":"string","enum":["read_only","edit"],"default":"read_only"},"max_steps":{"type":"integer"}},"required":["description","prompt"]})),
    ]
}

/// Specs available in a context (subagents get no `task`).
pub fn specs_for(ctx: &ToolContext) -> Vec<ToolSpec> {
    let plan = ctx.mode() == PermissionMode::Plan;
    let mut v: Vec<ToolSpec> = all_specs()
        .into_iter()
        .filter(|s| s.name != "task" || ctx.subagents.is_some())
        .filter(|s| s.name != "exit_plan_mode" || plan)
        .collect();
    if let Some(m) = &ctx.mcp {
        v.extend(m.specs());
    }
    v
}

fn canonical_name(name: &str) -> &str {
    match name {
        "patch_file" | "search_replace" | "str_replace" | "edit" => "edit_file",
        "search_files" | "search" | "rg" => "grep",
        "read" | "view" => "read_file",
        "write" | "create_file" => "write_file",
        "ls" => "list_dir",
        other => other,
    }
}

/// Execute one tool call. Errors are returned as `Err` and should be fed back
/// to the model as an error observation (never panic, never abort the turn).
pub async fn execute(ctx: &ToolContext, call: &ToolCall) -> Result<String> {
    if ctx.hooks.is_empty() {
        return dispatch(ctx, call).await;
    }
    let root = ctx.workspace.root().to_path_buf();
    let notes = match ctx.hooks.pre_tool(&root, &call.tool, &call.args).await {
        crate::hooks::PreOutcome::Block(reason) => return Err(anyhow!("{}", reason)),
        crate::hooks::PreOutcome::Continue(n) => n,
    };
    let res = dispatch(ctx, call).await;
    let (out, is_err) = match &res {
        Ok(o) => (o.clone(), false),
        Err(e) => (e.to_string(), true),
    };
    let (post_notes, fail) = ctx.hooks.post_tool(&root, &call.tool, &call.args, &out, is_err).await;
    let mut extra: Vec<String> = notes.into_iter().map(|n| format!("[pre_tool hook] {}", n)).collect();
    extra.extend(post_notes);
    let joined = if extra.is_empty() { String::new() } else { format!("\n\n{}", extra.join("\n")) };
    match (res, fail) {
        (Ok(o), None) => Ok(format!("{}{}", o, joined)),
        (Ok(o), Some(f)) => Err(anyhow!("{}{}\n\n{}", o, joined, f)),
        (Err(e), _) => Err(anyhow!("{}{}", e, joined)),
    }
}

async fn dispatch(ctx: &ToolContext, call: &ToolCall) -> Result<String> {
    let args = &call.args;
    let out = match canonical_name(&call.tool) {
        "read_file" => fs::read_file(ctx, args),
        "write_file" => fs::write_file(ctx, args).await,
        "edit_file" => fs::edit_file(ctx, &call.tool, args).await,
        "bash" => bash::bash(ctx, args).await,
        "bash_output" => bash::bash_output(ctx, args),
        "kill_shell" => bash::kill_shell(ctx, args),
        "grep" => {
            // legacy search_files callers passed `dir` and expected file names
            if call.tool == "search_files" && args.get("output_mode").is_none() {
                let mut a = args.clone();
                a["output_mode"] = json!("files_with_matches");
                search::grep(ctx, &a)
            } else {
                search::grep(ctx, args)
            }
        }
        "glob" => search::glob(ctx, args),
        "list_dir" => search::list_dir(ctx, args),
        "find_symbol" => ctx.symbols_find(args),
        "todo_write" => planning::todo_write(ctx, args),
        "web_fetch" | "fetch" => web::web_fetch(ctx, args).await,
        "exit_plan_mode" => planning::exit_plan_mode(ctx, args).await,
        "task" => match &ctx.subagents {
            Some(sp) => sp.spawn(args.clone(), ctx.clone()).await,
            None => Err(anyhow!("task is not available here (subagents cannot spawn subagents)")),
        },
        other if other.starts_with("mcp__") => match &ctx.mcp {
            Some(m) => m.call(ctx, other, args).await,
            None => Err(anyhow!("MCP is not configured; unknown tool {}", other)),
        },
        other => Err(anyhow!("Unknown tool: {}. Available: {}", other, specs_for(ctx).iter().map(|s| s.name.clone()).collect::<Vec<_>>().join(", "))),
    }?;
    Ok(clamp_default(&out))
}

#[cfg(test)]
mod tests;
