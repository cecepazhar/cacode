//! Non-interactive mode: `cacode -p "task"` for CI, scripts and other agents.
//!
//! * `--output-format text` (default) prints the final answer.
//! * `json` prints one result object; `stream-json` prints every event as a
//!   JSON line (NDJSON) followed by the result object.
//! * No human can answer approvals: in `ask` mode edits / non-allowlisted
//!   commands are denied (the agent is told why). Use `--permission-mode
//!   auto-edit` or `yolo` for unattended changes.
//! * Exit codes: 0 finished · 1 provider/turn failure · 2 step/loop/token
//!   budget stop · 130 cancelled (Ctrl+C).

use crate::commands::chat::{ReActAgent, TurnOutcome};
use crate::events::AgentEvent;
use crate::llm::Credentials;
use crate::permission::PermissionMode;
use crate::sessions::SessionStore;
use anyhow::{anyhow, Result};
use serde_json::json;
use std::io::Read;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl OutputFormat {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" | "ndjson" => Ok(Self::StreamJson),
            o => Err(anyhow!("unknown --output-format '{}': use text | json | stream-json", o)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub prompt: String,
    pub format: OutputFormat,
    pub permission_mode: Option<PermissionMode>,
    pub max_steps: Option<usize>,
    pub continue_last: bool,
    pub resume: Option<String>,
    pub model: Option<String>,
}

pub fn event_json(ev: &AgentEvent) -> Option<serde_json::Value> {
    Some(match ev {
        AgentEvent::Thinking(t) => json!({"type": "status", "text": t}),
        AgentEvent::AssistantDelta(d) => json!({"type": "delta", "text": d}),
        AgentEvent::AssistantChunk(c) => json!({"type": "assistant", "text": c}),
        AgentEvent::ToolStart { tool, args } => json!({"type": "tool_start", "tool": tool, "args": serde_json::from_str::<serde_json::Value>(args).unwrap_or(json!(args))}),
        AgentEvent::ToolOutput { tool, output, is_error } => json!({"type": "tool_result", "tool": tool, "is_error": is_error, "output": crate::text::clamp_output(output, 4_000, 80)}),
        AgentEvent::Compacted(m) => json!({"type": "compacted", "text": m}),
        AgentEvent::Subagent { id, description, status } => json!({"type": "subagent", "id": id, "description": description, "status": format!("{:?}", status)}),
        AgentEvent::Todos(t) => json!({"type": "todos", "todos": t}),
        AgentEvent::ModeChanged(m) => json!({"type": "mode", "mode": m.label()}),
        AgentEvent::Error(e) => json!({"type": "error", "text": e}),
        AgentEvent::Cancelled => json!({"type": "cancelled"}),
        AgentEvent::ApprovalRequired(t) => json!({"type": "approval_denied", "summary": t.request.summary}),
        AgentEvent::TurnFinished { .. } | AgentEvent::HistorySync { .. } => return None,
    })
}

fn read_prompt(p: &str) -> Result<String> {
    if p == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        Ok(p.to_string())
    }
}

/// Run one headless turn. Returns the process exit code.
pub async fn run(opts: HeadlessOptions) -> Result<i32> {
    let cfg = crate::config::CaCodeConfig::load().unwrap_or_default();
    let mut agent = ReActAgent::with_credentials(Credentials::from_config(&cfg));
    if let Some(m) = PermissionMode::parse(&cfg.permission_mode) {
        agent.permission_mode = m;
    }
    if let Some(m) = opts.permission_mode {
        agent.permission_mode = m;
    }
    agent.max_steps = opts.max_steps.unwrap_or(cfg.max_steps.max(1));
    agent.subagent_limit = if cfg.is_pro() { 49 } else { 5 };
    agent.context_window_override = (cfg.context_window > 0).then_some(cfg.context_window);
    agent.sandbox = cfg.sandbox_config();
    agent.web_allow_domains = cfg.web_allow_domains.clone();
    if let Some(model) = &opts.model {
        set_model(&mut agent.provider, model);
    }
    let store = SessionStore::open_default();
    let ws = std::env::current_dir()?.canonicalize()?;
    if let Some(id) = &opts.resume {
        agent.restore_session(store.load(id)?);
    } else if opts.continue_last {
        match store.latest_for(&ws) {
            Some(rec) => agent.restore_session(rec),
            None => eprintln!("[cacode] no previous session for {} — starting a new one", ws.display()),
        }
    }
    let prompt = read_prompt(&opts.prompt)?;
    if prompt.trim().is_empty() {
        return Err(anyhow!("empty prompt"));
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let (_steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let ctrlc = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            c2.cancel();
        }
    });
    let started = Instant::now();
    let mut worker = agent;
    let handle = tokio::spawn(async move {
        worker.event_tx = Some(tx);
        worker.add_user(&prompt);
        let outcome = worker.run_loop(Some(steer_rx), cancel).await;
        let _ = worker.save_session(&SessionStore::open_default());
        worker.detach_events();
        (worker, outcome)
    });

    let mut tools: Vec<serde_json::Value> = Vec::new();
    let mut last_text = String::new();
    while let Some(ev) = rx.recv().await {
        if let AgentEvent::ApprovalRequired(t) = &ev {
            // nobody can answer: deny explicitly so the agent can adapt
            t.respond(crate::events::ApprovalDecision::Reject(Some("headless mode: no interactive approver (use --permission-mode auto-edit or yolo)".into())));
        }
        match &ev {
            AgentEvent::ToolOutput { tool, is_error, .. } => tools.push(json!({"tool": tool, "is_error": is_error})),
            AgentEvent::AssistantChunk(c) if !c.trim().is_empty() => last_text = c.clone(),
            AgentEvent::Thinking(t) if opts.format == OutputFormat::Text => eprintln!("· {}", t),
            AgentEvent::ToolStart { tool, args } if opts.format == OutputFormat::Text => eprintln!("⚙ {} {}", tool, crate::text::truncate_chars(args, 160)),
            _ => {}
        }
        if opts.format == OutputFormat::StreamJson {
            if let Some(j) = event_json(&ev) {
                println!("{}", j);
            }
        }
    }
    ctrlc.abort();
    let (agent, outcome) = handle.await.map_err(|e| anyhow!("agent task failed: {}", e))?;
    let (label, code) = match &outcome {
        TurnOutcome::Finished => ("finished".to_string(), 0),
        TurnOutcome::StepBudget => ("step_budget".into(), 2),
        TurnOutcome::LoopDetected => ("loop_detected".into(), 2),
        TurnOutcome::TokenBudget => ("token_budget".into(), 2),
        TurnOutcome::Cancelled => ("cancelled".into(), 130),
        TurnOutcome::Failed(e) => (format!("failed: {}", e), 1),
    };
    let files: Vec<String> = agent.tool_ctx.as_ref().and_then(|c| c.files_touched.lock().ok().map(|s| s.iter().cloned().collect())).unwrap_or_default();
    match opts.format {
        OutputFormat::Text => {
            if !last_text.is_empty() {
                println!("{}", last_text);
            }
            if code != 0 {
                eprintln!("[cacode] {}", label);
            }
            eprintln!("[cacode] session {} · {} tokens{} · {:.1}s", agent.session_id, agent.total_usage.total_tokens, if agent.total_usage.estimated { " (est.)" } else { "" }, started.elapsed().as_secs_f64());
        }
        _ => {
            println!(
                "{}",
                json!({
                    "type": "result",
                    "outcome": label,
                    "exit_code": code,
                    "result": last_text,
                    "session_id": agent.session_id,
                    "provider": agent.provider.display_label(),
                    "permission_mode": agent.permission_mode.label(),
                    "usage": agent.total_usage,
                    "llm_calls": agent.total_calls,
                    "duration_ms": started.elapsed().as_millis() as u64,
                    "tool_calls": tools,
                    "files_touched": files,
                })
            );
        }
    }
    Ok(code)
}

pub fn set_model(p: &mut crate::commands::chat::Provider, m: &str) {
    use crate::commands::chat::Provider::*;
    match p {
        OpenAI { model } | Anthropic { model } | Ollama { model } | Router9 { model } | AntigravityZen { model } | Custom { model, .. } => *model = m.to_string(),
    }
}

/// `cacode sessions` listing.
pub fn list_sessions(all: bool) -> Result<()> {
    let store = SessionStore::open_default();
    let ws = std::env::current_dir()?.canonicalize()?;
    let list = store.list(if all { None } else { Some(&ws) });
    if list.is_empty() {
        println!("No saved sessions{}.", if all { "" } else { " for this directory (use --all)" });
        return Ok(());
    }
    for s in list.iter().take(50) {
        println!("{}  {:>4} msgs  {}  {}", s.id, s.messages, s.updated.get(..16).unwrap_or(&s.updated), s.title);
    }
    println!("\nResume: cacode --resume <id>   ·   latest here: cacode --continue");
    Ok(())
}
