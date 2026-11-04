//! Events streamed from the background agent worker to a front-end (TUI,
//! REPL, or a parent agent), plus the approval hand-shake.

use crate::permission::PermissionMode;
use crate::types::{Message, TodoEntry, TokenUsage};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub type AgentEvents = mpsc::UnboundedSender<AgentEvent>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    /// Approve and stop asking for this kind of action for the rest of the session.
    ApproveAlways,
    Reject(Option<String>),
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: u64,
    pub tool: String,
    /// One-line human summary, e.g. `Edit src/main.rs` or `Run: cargo test`.
    pub summary: String,
    /// Unified diff for file edits (already colourless text).
    pub diff: Option<String>,
}

/// Clone-able one-shot responder so `AgentEvent` can stay `Clone`.
#[derive(Clone)]
pub struct ApprovalTicket {
    pub request: ApprovalRequest,
    responder: Arc<Mutex<Option<oneshot::Sender<ApprovalDecision>>>>,
}

impl std::fmt::Debug for ApprovalTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalTicket").field("request", &self.request).finish()
    }
}

impl ApprovalTicket {
    pub fn new(request: ApprovalRequest) -> (Self, oneshot::Receiver<ApprovalDecision>) {
        let (tx, rx) = oneshot::channel();
        (Self { request, responder: Arc::new(Mutex::new(Some(tx))) }, rx)
    }

    /// Answer the request. Returns false if it was already answered or the agent is gone.
    pub fn respond(&self, decision: ApprovalDecision) -> bool {
        match self.responder.lock().ok().and_then(|mut g| g.take()) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }
}

/// Snapshot of a subagent's lifecycle, for progress display.
#[derive(Debug, Clone)]
pub enum SubagentStatus {
    Started,
    Step(usize),
    Finished { verified: bool },
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Thinking(String),
    /// Incremental streamed text of the assistant message being generated.
    AssistantDelta(String),
    /// The complete assistant text of one ReAct step (after streaming ends).
    AssistantChunk(String),
    ToolStart { tool: String, args: String },
    ToolOutput { tool: String, output: String, is_error: bool },
    ApprovalRequired(ApprovalTicket),
    Subagent { id: usize, description: String, status: SubagentStatus },
    /// Context was compacted automatically.
    Compacted(String),
    /// The agent replaced its task list (`todo_write`).
    Todos(Vec<TodoEntry>),
    /// Permission mode changed during the turn (e.g. plan approved).
    ModeChanged(PermissionMode),
    TurnFinished {
        usage: TokenUsage,
        total_calls: usize,
        duration_ms: u64,
        history: Vec<Message>,
        total_usage: TokenUsage,
        last_usage: TokenUsage,
    },
    /// The turn failed. `history` carries everything that happened before the
    /// failure so the front-end does not lose tool effects already applied.
    Error(String),
    Cancelled,
    /// Emitted right before `Error` / `Cancelled` so state can be synced.
    HistorySync { history: Vec<Message>, total_usage: TokenUsage, last_usage: TokenUsage, total_calls: usize },
}
