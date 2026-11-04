//! CACode ReAct agent: native tool calling, streaming, typed provider
//! errors with backoff / failover / circuit breaker, auto-compaction,
//! permissioned sandboxed tools and parallel subagents.

use crate::events::ApprovalDecision;
use crate::llm::retry::{CircuitBreaker, RetryPolicy};
use crate::llm::{self, Credentials, LlmError, LlmRequest, LlmResponse, Timeouts, ToolDef, ToolMode};
use crate::permission::PermissionMode;
use crate::text::{clamp_output, estimate_tokens};
use crate::tools::{self, ToolContext, Workspace};
use anyhow::{anyhow, Result};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use crate::events::{AgentEvent, AgentEvents};
pub use crate::types::{Message, TokenUsage, ToolCall};

pub const CA_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_MAX_STEPS: usize = 25;
pub const AUTO_COMPACT_RATIO: f64 = 0.80;
pub const KEEP_RECENT_MESSAGES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provider {
    OpenAI { model: String },
    Anthropic { model: String },
    Ollama { model: String },
    Router9 { model: String },
    AntigravityZen { model: String },
    Custom { name: String, base_url: String, api_key: String, model: String },
}

impl Provider {
    pub fn name(&self) -> &str {
        match self {
            Provider::OpenAI { .. } => "OpenAI",
            Provider::Anthropic { .. } => "Anthropic",
            Provider::Ollama { .. } => "Ollama",
            Provider::Router9 { .. } => "9Router",
            Provider::AntigravityZen { .. } => "Antigravity Zen",
            Provider::Custom { name, .. } => name.as_str(),
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Provider::OpenAI { model }
            | Provider::Anthropic { model }
            | Provider::Ollama { model }
            | Provider::Router9 { model }
            | Provider::AntigravityZen { model }
            | Provider::Custom { model, .. } => model.as_str(),
        }
    }

    pub fn display_label(&self) -> String {
        format!("{} ({})", self.name(), self.model())
    }
}


/// How a turn ended (used by subagents and tests).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Finished,
    StepBudget,
    LoopDetected,
    TokenBudget,
    Cancelled,
    Failed(String),
}

#[derive(Clone)]
pub struct ReActAgent {
    pub client: Client,
    pub history: Vec<Message>,
    pub provider: Provider,
    pub fallback_chain: Vec<Provider>,
    pub max_retries: usize,
    pub event_tx: Option<AgentEvents>,
    pub user_name: String,
    pub persona: String,
    pub thinking_mode: String,
    /// Context window of the active model (tokens). Refreshed every call
    /// unless `context_window_override` is set.
    pub context_window_limit: u64,
    pub context_window_override: Option<u64>,
    pub last_usage: TokenUsage,
    pub total_usage: TokenUsage,
    pub total_calls: usize,
    // --- runtime configuration -------------------------------------------
    pub creds: Credentials,
    pub permission_mode: PermissionMode,
    /// Workspace root; `None` = current directory at turn start.
    pub workspace_root: Option<PathBuf>,
    pub max_steps: usize,
    pub max_tokens: u32,
    pub retry: RetryPolicy,
    pub timeouts: Timeouts,
    pub breaker: CircuitBreaker,
    pub stream: bool,
    /// Tool mode per provider label, learned at runtime (`ToolsUnsupported`).
    pub tool_modes: Arc<Mutex<HashMap<String, ToolMode>>>,
    pub tool_ctx: Option<ToolContext>,
    /// Max concurrently running subagents (free 5, pro 49).
    pub subagent_limit: usize,
    /// Token budget for one turn (subagent watchdog). 0 = unlimited.
    pub turn_token_budget: i64,
    pub is_subagent: bool,
    pub inject_repo_context: bool,
    pub record_usage: bool,
    /// Persistent session id (`~/.cacode/sessions/<id>.json`).
    pub session_id: String,
    pub session_created: String,
    pub extensions_announced: bool,
    /// bubblewrap sandbox for `bash` (applied when the tool context is created).
    pub sandbox: crate::tools::bash::SandboxConfig,
    /// Extra domains web_fetch may read without approval.
    pub web_allow_domains: Vec<String>,
    /// Save the session after every turn.
    pub autosave: bool,
}

impl Default for ReActAgent {
    fn default() -> Self {
        Self::new()
    }
}

fn provider_key(p: &Provider) -> String {
    p.display_label()
}

impl ReActAgent {
    pub fn detect_provider() -> Provider {
        Self::detect_provider_with(&Credentials::from_env())
    }

    pub fn detect_provider_with(c: &Credentials) -> Provider {
        if c.anthropic_key.is_some() {
            let model = env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-20250514".into());
            Provider::Anthropic { model }
        } else if c.openai_key.is_some() {
            let model = env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into());
            Provider::OpenAI { model }
        } else if c.zen_key.is_some() {
            let model = env::var("ANTIGRAVITY_ZEN_MODEL")
                .or_else(|_| env::var("ANTIGRAVITY_MODEL"))
                .unwrap_or_else(|_| "deepseek-v4-flash-free".into());
            Provider::AntigravityZen { model }
        } else if env::var("OLLAMA_MODEL").is_ok() || c.ollama_url.is_some() {
            let model = env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".into());
            Provider::Ollama { model }
        } else {
            let model = env::var("ROUTER9_MODEL")
                .map(|m| if m == "default" || m.is_empty() { "ZA126_PRO".to_string() } else { m })
                .unwrap_or_else(|_| "ZA126_PRO".into());
            Provider::Router9 { model }
        }
    }

    pub fn new() -> Self {
        Self::with_credentials(Credentials::from_env())
    }

    pub fn with_credentials(creds: Credentials) -> Self {
        let provider = Self::detect_provider_with(&creds);
        let mode = env::var("CACODE_PERMISSION_MODE").ok().and_then(|m| PermissionMode::parse(&m)).unwrap_or_default();
        Self {
            client: llm::build_client(),
            history: Vec::new(),
            context_window_limit: crate::pricing::context_window(provider.model()),
            provider,
            fallback_chain: Vec::new(),
            max_retries: 3,
            event_tx: None,
            user_name: "Prof. Cecep".into(),
            persona: "You are CACode, an elite sovereign Rust development assistant. The user is Prof. Cecep (Cecep Saeful Azhar Hidayat, ST). Always greet and address him respectfully as 'Prof. Cecep' (e.g. 'Hi Prof. Cecep', 'Siap, Prof. Cecep.'). Be direct, concise, and technically precise.".into(),
            thinking_mode: "Fast".into(),
            context_window_override: None,
            last_usage: TokenUsage::default(),
            total_usage: TokenUsage::default(),
            total_calls: 0,
            creds,
            permission_mode: mode,
            workspace_root: None,
            max_steps: env::var("CACODE_MAX_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_MAX_STEPS),
            max_tokens: 8192,
            retry: RetryPolicy::default(),
            timeouts: Timeouts::default(),
            breaker: CircuitBreaker::default(),
            stream: true,
            tool_modes: Arc::default(),
            tool_ctx: None,
            subagent_limit: 5,
            turn_token_budget: 0,
            is_subagent: false,
            inject_repo_context: true,
            record_usage: true,
            session_id: crate::sessions::new_id(),
            extensions_announced: false,
            sandbox: crate::tools::bash::SandboxConfig::from_settings("auto", &[], true),
            web_allow_domains: Vec::new(),
            session_created: chrono::Utc::now().to_rfc3339(),
            autosave: true,
        }
    }

    fn workspace_path(&self) -> PathBuf {
        let root = self.workspace_root.clone().unwrap_or_else(|| env::current_dir().unwrap_or_default());
        root.canonicalize().unwrap_or(root)
    }

    /// Snapshot of this conversation for the session store.
    pub fn session_record(&self) -> crate::sessions::SessionRecord {
        crate::sessions::SessionRecord {
            id: self.session_id.clone(),
            title: crate::sessions::SessionStore::title_from(&self.history),
            workspace: self.workspace_path(),
            created: self.session_created.clone(),
            updated: chrono::Utc::now().to_rfc3339(),
            provider: self.provider.display_label(),
            history: self.history.clone(),
            total_usage: self.total_usage.clone(),
        }
    }

    pub fn save_session(&self, store: &crate::sessions::SessionStore) -> Result<()> {
        if self.history.is_empty() || self.is_subagent {
            return Ok(());
        }
        store.save(&self.session_record())
    }

    /// Continue a stored conversation in this agent.
    pub fn restore_session(&mut self, rec: crate::sessions::SessionRecord) {
        self.session_id = rec.id;
        self.session_created = rec.created;
        self.history = rec.history;
        self.total_usage = rec.total_usage;
        // a resumed tool_call without its result would make the next request invalid
        let pending: Vec<ToolCall> = match self.history.last() {
            Some(m) if m.role == "assistant" && !m.tool_calls.is_empty() => m.tool_calls.clone(),
            _ => Vec::new(),
        };
        for c in pending {
            self.history.push(Message::tool_result(&c, "Not executed: the session was interrupted before this tool ran.", true));
        }
    }

    pub fn add_user(&mut self, content: &str) {
        self.history.push(Message::user(content));
    }

    pub fn add_assistant(&mut self, content: &str) {
        self.history.push(Message::assistant(content));
    }

    pub fn context_limit(&self) -> u64 {
        self.context_window_override.unwrap_or_else(|| crate::pricing::context_window(self.provider.model()))
    }

    fn tool_mode_for(&self, p: &Provider) -> ToolMode {
        if env::var("CACODE_TOOL_MODE").map(|v| v.eq_ignore_ascii_case("text")).unwrap_or(false) {
            return ToolMode::Text;
        }
        self.tool_modes.lock().ok().and_then(|m| m.get(&provider_key(p)).copied()).unwrap_or(ToolMode::Native)
    }

    fn text_mode_instructions() -> &'static str {
        "\n\n# Tool calling (text mode)\nWhen you need tools, output ONLY a JSON array strictly wrapped in a ```json fenced block, e.g.\n```json\n[{\"tool\": \"read_file\", \"args\": {\"path\": \"src/main.rs\"}}]\n```\nYou will receive each result as an `Observation from tool ...` message. You may also edit with Aider blocks:\npath/to/file.rs\n<<<<<<< SEARCH\n[exact old lines]\n=======\n[new lines]\n>>>>>>> REPLACE\nWhen no tool is needed, answer in natural language without any JSON block."
    }

    /// System prompt for the given tool mode (repo context appended separately).
    pub fn system_prompt_for(&self, mode: ToolMode, ctx: Option<&ToolContext>) -> String {
        let persona_intro = if !self.persona.is_empty() {
            self.persona.clone()
        } else {
            format!(
                "You are CACode, a sovereign Rust-focused development agent. The user is {}. Address the user respectfully as {} when appropriate.",
                self.user_name, self.user_name
            )
        };
        let thinking_hint = match self.thinking_mode.as_str() {
            "Analytical" => "\n\n[Cognitive Mode: Analytical — analyze problems with structured step-by-step logic before answering or using tools.]",
            "Deep" => "\n\n[Cognitive Mode: Deep Reasoning — apply comprehensive adversarial verification, edge-case proofs, and rigorous architectural checks.]",
            _ => "",
        };
        let specs = match ctx {
            Some(c) => tools::specs_for(c),
            None => tools::all_specs(),
        };
        let tool_list: String = specs
            .iter()
            .map(|s| if mode == ToolMode::Text { format!("- {}: {} Args schema: {}\n", s.name, s.description, s.schema) } else { format!("- {}\n", s.name) })
            .collect();
        let mut p = format!(
            "{persona_intro}{thinking_hint}\n\n# Operating rules\n\
             - You work inside the user's workspace through tools. Never claim you ran, tested or changed something unless a tool result in this conversation shows it.\n\
             - Explore before editing: use grep / glob / find_symbol / read_file. Read a file before editing it; copy old_string exactly from read_file output without the line-number prefix.\n\
             - Prefer small, precise edit_file calls over rewriting whole files.\n\
             - After changing code, verify with the project's build/test command via bash and report the actual exit code.\n\
             - Tool results may be truncated (marked). Page with offset/limit or narrow searches instead of re-running blindly.\n\
             - If a tool fails, read the error, adjust, and retry differently; do not repeat an identical failing call.\n\
             - Permission mode: {mode_label}. Some actions may be rejected by the user; respect rejections.\n\
             - Use `task` to parallelise independent read-only investigations.\n\n# Tools\n{tool_list}",
            mode_label = self.permission_mode.label()
        );
        if self.permission_mode == PermissionMode::Plan {
            p.push_str("\n# PLAN MODE\nYou may only read and investigate (read_file, grep, glob, find_symbol, allowlisted bash). Do NOT try to edit files. When you understand the task, call exit_plan_mode with a concise, step-by-step plan (files to change, approach, how you will verify). Wait for approval.\n");
        }
        if self.is_subagent {
            p.push_str("\n# You are a SUBAGENT\nYou were spawned by a parent agent for one self-contained task. Work autonomously (no user is watching). When done, reply WITHOUT tool calls and end your final message with a JSON object in a ```json fence: {\"summary\": \"what you found/did\", \"claims\": [\"each factual claim\"], \"files\": [\"paths you inspected or changed\"]}. Only claim tests pass / bugs fixed if a bash result above proves it.\n");
        }
        if mode == ToolMode::Text {
            p.push_str(Self::text_mode_instructions());
        }
        p
    }

    /// Backwards-compatible accessor (native mode, no repo context).
    pub fn system_prompt(&self) -> Message {
        Message::system(self.system_prompt_for(ToolMode::Native, self.tool_ctx.as_ref()))
    }

    /// Legacy helper kept for callers that parse raw provider bodies.
    pub fn extract_response_content(text: &str) -> Result<(String, TokenUsage)> {
        let r = crate::llm::sse::parse_full_body(text, crate::llm::Wire::OpenAi).map_err(|e| anyhow!("API error: {}", e))?;
        Ok((r.content, r.usage))
    }

    pub fn record_usage_to_sqlite(model: &str, usage: &TokenUsage) {
        crate::usage_db::record_agent_usage("", model, usage);
    }

    // ------------------------------------------------------------ context ----

    /// Get (or lazily create) the session tool context for the current workspace.
    pub fn ensure_tool_ctx(&mut self) -> Result<ToolContext> {
        let root = match &self.workspace_root {
            Some(r) => r.clone(),
            None => env::current_dir()?,
        };
        let root = root.canonicalize().unwrap_or(root);
        let same_root = self.tool_ctx.as_ref().map(|c| c.workspace.root() == root).unwrap_or(false);
        if same_root {
            let mode = self.permission_mode;
            if let Some(c) = self.tool_ctx.as_mut() {
                if c.mode() != mode {
                    c.set_mode(mode);
                }
            }
        } else {
            let mut ctx = ToolContext::new(Workspace::new(&root)?, self.permission_mode);
            ctx.sandbox = self.sandbox.clone();
            ctx.web_allowlist.extend(self.web_allow_domains.iter().cloned());
            self.tool_ctx = Some(if self.is_subagent { ctx } else { ctx.with_extensions() });
        }
        Ok(self.tool_ctx.clone().expect("set above"))
    }

    fn repo_context(&self, ctx: &ToolContext) -> String {
        if !self.inject_repo_context {
            return String::new();
        }
        let mut guard = match ctx.symbols.lock() {
            Ok(g) => g,
            Err(_) => return String::new(),
        };
        if guard.is_none() {
            *guard = Some(crate::symbols::SymbolIndex::load_or_build(ctx.workspace.root()));
        } else if let Some(idx) = guard.as_mut() {
            idx.refresh();
        }
        let opts = crate::repo_context::RepoContextOptions { map_budget: if self.is_subagent { 1_500 } else { 3_000 }, ..Default::default() };
        crate::repo_context::build(ctx.workspace.root(), guard.as_ref(), &opts)
    }

    fn estimate_prompt_tokens(&self, system: &str) -> i64 {
        estimate_tokens(system)
            + self.history.iter().map(|m| estimate_tokens(&m.content) + m.tool_calls.iter().map(|c| estimate_tokens(&c.args.to_string()) + 10).sum::<i64>() + 4).sum::<i64>()
    }

    // ---------------------------------------------------------- LLM calls ----

    fn ordered_providers(&self) -> Vec<Provider> {
        let mut all = vec![self.provider.clone()];
        for fb in &self.fallback_chain {
            if !all.contains(fb) {
                all.push(fb.clone());
            }
        }
        let (closed, open): (Vec<_>, Vec<_>) = all.into_iter().partition(|p| !self.breaker.is_open(&provider_key(p)));
        // try healthy providers first; open (cooling-down) ones last rather than never
        closed.into_iter().chain(open).collect()
    }

    fn emit(&self, ev: AgentEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(ev);
        }
    }

    async fn sleep_or_cancel(d: Duration, cancel: Option<&CancellationToken>) -> bool {
        match cancel {
            Some(c) => tokio::select! { _ = tokio::time::sleep(d) => true, _ = c.cancelled() => false },
            None => {
                tokio::time::sleep(d).await;
                true
            }
        }
    }

    /// One model call with retry, failover, tool-mode downgrade.
    /// Returns the response, the provider that served it, and its tool mode.
    async fn call_model(
        &mut self,
        system_extra: &str,
        tools_on: bool,
        cancel: Option<&CancellationToken>,
    ) -> std::result::Result<(LlmResponse, Provider, ToolMode), LlmError> {
        self.context_window_limit = self.context_limit();
        let providers = self.ordered_providers();
        let mut last_err = LlmError::Unavailable("no provider configured".into());
        let ctx = self.tool_ctx.clone();
        let tool_defs: Vec<ToolDef> = if tools_on {
            ctx.as_ref()
                .map(tools::specs_for)
                .unwrap_or_else(tools::all_specs)
                .into_iter()
                .map(|s| ToolDef { name: s.name, description: s.description, schema: s.schema })
                .collect()
        } else {
            Vec::new()
        };

        for (tier, prov) in providers.iter().enumerate() {
            let key = provider_key(prov);
            let mut attempt = 0usize;
            let mut downgraded = false;
            loop {
                attempt += 1;
                let mode = if tools_on { self.tool_mode_for(prov) } else { ToolMode::Native };
                let system = format!("{}{}", self.system_prompt_for(mode, ctx.as_ref()), system_extra);
                let req = LlmRequest {
                    model: prov.model(),
                    system: &system,
                    messages: &self.history,
                    tools: if mode == ToolMode::Native { &tool_defs } else { &[] },
                    tool_mode: mode,
                    max_tokens: self.max_tokens,
                    stream: self.stream,
                };
                let tx = self.event_tx.clone();
                let on_delta = move |d: &str| {
                    if let Some(tx) = &tx {
                        let _ = tx.send(AgentEvent::AssistantDelta(d.to_string()));
                    }
                };
                self.total_calls += 1;
                let res = llm::complete(&self.client, prov, &self.creds, &req, self.timeouts, &on_delta).await;
                match res {
                    Ok(r) => {
                        self.breaker.reset(&key);
                        if tier > 0 {
                            self.emit(AgentEvent::Thinking(format!("↪ served by fallback {}", prov.display_label())));
                        }
                        return Ok((r, prov.clone(), mode));
                    }
                    Err(LlmError::ToolsUnsupported(m)) if !downgraded => {
                        downgraded = true;
                        if let Ok(mut tm) = self.tool_modes.lock() {
                            tm.insert(key.clone(), ToolMode::Text);
                        }
                        self.emit(AgentEvent::Thinking(format!("{} rejected native tools ({}); switching to text tool-mode", prov.name(), crate::text::truncate_chars(&m, 80))));
                        attempt -= 1;
                        continue;
                    }
                    Err(e) => {
                        let retry = e.retryable() && attempt < self.max_retries.max(1);
                        last_err = e.clone();
                        if retry {
                            let d = self.retry.delay(attempt, &e);
                            self.emit(AgentEvent::Thinking(format!("{} — {} (retry {}/{} in {:.1}s)", prov.name(), crate::text::truncate_chars(&e.to_string(), 120), attempt, self.max_retries, d.as_secs_f64())));
                            if !Self::sleep_or_cancel(d, cancel).await {
                                return Err(LlmError::Network("cancelled".into()));
                            }
                            continue;
                        }
                        if !e.failover() {
                            return Err(e);
                        }
                        break;
                    }
                }
            }
            self.breaker.trip(&key, self.retry.cooldown);
            if let Some(next) = providers.get(tier + 1) {
                let msg = format!("⚠️ {} failed ({}). Failing over to Fallback Tier {} ({})", prov.name(), crate::text::truncate_chars(&last_err.to_string(), 120), tier + 1, next.name());
                self.emit(AgentEvent::Thinking(msg));
            }
        }
        Err(last_err)
    }

    fn account(&mut self, prov: &Provider, usage: &TokenUsage) {
        self.last_usage = usage.clone();
        self.total_usage.add(usage);
        if self.record_usage {
            crate::usage_db::record_agent_usage(prov.name(), prov.model(), usage);
        }
    }

    /// Plain completion without tools or history (summaries, legacy callers).
    pub async fn call_llm(&mut self, user_input: &str) -> Result<String> {
        let saved = std::mem::replace(&mut self.history, vec![Message::user(user_input)]);
        let res = self.call_model("", false, None).await;
        self.history = saved;
        let (r, prov, _) = res.map_err(|e| anyhow!("All providers failed (primary + {} fallbacks). Last error: {}", self.fallback_chain.len(), e))?;
        self.account(&prov, &r.usage);
        Ok(r.content)
    }

    // --------------------------------------------------------- compaction ----

    fn render_for_summary(msgs: &[Message]) -> String {
        msgs.iter()
            .map(|m| {
                let mut s = format!("[{}{}]: {}", m.role, m.name.as_deref().map(|n| format!(" {}", n)).unwrap_or_default(), if m.role == "tool" { clamp_output(&m.content, 2_000, 40) } else { m.content.clone() });
                for c in &m.tool_calls {
                    s.push_str(&format!("\n  -> call {}({})", c.tool, crate::text::truncate_chars(&c.args.to_string(), 300)));
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Index where the kept tail starts: a plain user message, so no tool
    /// call is ever separated from its result.
    fn compaction_split(&self, keep_recent: usize) -> usize {
        let n = self.history.len();
        let mut i = n.saturating_sub(keep_recent);
        while i > 0 && !(self.history[i].role == "user") {
            i -= 1;
        }
        i
    }

    /// Summarise everything but the recent tail. `force` ignores the threshold.
    /// Returns a status line when something was compacted.
    pub async fn compact(&mut self, force: bool) -> Result<Option<String>> {
        let limit = self.context_limit() as i64;
        let est = self.estimate_prompt_tokens("");
        if !force && (est as f64) < limit as f64 * AUTO_COMPACT_RATIO {
            return Ok(None);
        }
        let mut split = self.compaction_split(KEEP_RECENT_MESSAGES);
        if split == 0 && self.history.len() > 2 {
            // a single giant turn: keep only from the latest user message
            split = self.compaction_split(1);
        }
        if split == 0 {
            // nothing safe to summarise: shrink old tool outputs instead
            let mut shrunk = 0;
            let n = self.history.len();
            for m in self.history.iter_mut().take(n.saturating_sub(2)) {
                if m.role == "tool" && m.content.len() > 4_000 {
                    m.content = clamp_output(&m.content, 4_000, 80);
                    shrunk += 1;
                }
            }
            return Ok((shrunk > 0).then(|| format!("Shrunk {} large tool outputs", shrunk)));
        }
        let old: Vec<Message> = self.history[..split].to_vec();
        let before_tokens = est;
        let transcript = Self::render_for_summary(&old);
        // chunk so each summarisation request itself fits in the window
        let chunk_chars = ((limit as f64 * 0.45) * 3.5).clamp(8_000.0, 400_000.0) as usize;
        let mut summary = String::new();
        let mut rest = transcript.as_str();
        while !rest.is_empty() {
            let cut = crate::text::floor_char_boundary(rest, chunk_chars.min(rest.len()));
            let (chunk, tail) = rest.split_at(cut.max(1).min(rest.len()));
            rest = tail;
            let prompt = format!(
                "You compress coding-agent conversations. Produce a dense technical briefing that preserves: the user's goals and constraints, decisions made, files read/changed (exact paths), commands run with their exit codes, errors still open, and the current next step. Never invent facts. Begin with '[COMPACTED CONTEXT SUMMARY]'.\n\n{}Conversation segment:\n{}",
                if summary.is_empty() { String::new() } else { format!("Summary so far (merge it in):\n{}\n\n", summary) },
                chunk
            );
            summary = self.call_llm(&prompt).await?;
        }
        let kept: Vec<Message> = self.history[split..].to_vec();
        self.history = vec![
            Message::user(format!("{}\n\n(The conversation above this point was compacted automatically.)", summary)),
            Message::assistant("Understood — continuing from the compacted summary."),
        ];
        self.history.extend(kept);
        let after = self.estimate_prompt_tokens("");
        Ok(Some(format!(
            "Compacted {} older messages: ~{} → ~{} tokens ({}% of {} window)",
            split,
            before_tokens,
            after,
            (after * 100) / limit.max(1),
            limit
        )))
    }

    /// `/compress` entry point.
    pub async fn compress_history(&mut self) -> Result<String> {
        if self.history.len() <= 2 {
            return Ok("Context memory is already compact (less than 3 messages).".into());
        }
        match self.compact(true).await? {
            Some(s) => Ok(s),
            None => Ok("Nothing to compact.".into()),
        }
    }

    /// Text-mode tool-call parser (```json fences, bare JSON arrays, Aider
    /// SEARCH/REPLACE blocks). Used when the provider has no native tools or
    /// the model ignored them.
    pub fn parse_tool_calls(&self, content: &str) -> Vec<ToolCall> {
        static FENCE: OnceLock<Regex> = OnceLock::new();
        let json_block_re = FENCE.get_or_init(|| Regex::new(r"(?s)```json\s*(\[.*?\]|\{.*?\})\s*```").expect("static regex"));
        let mut fenced = Vec::new();
        for caps in json_block_re.captures_iter(content) {
            if let Some(mat) = caps.get(1) {
                let raw = mat.as_str();
                if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(raw) {
                    fenced.extend(calls);
                } else if let Ok(call) = serde_json::from_str::<ToolCall>(raw) {
                    fenced.push(call);
                }
            }
        }
        if !fenced.is_empty() {
            return fenced;
        }
        if let Ok(calls) = serde_json::from_str::<Vec<ToolCall>>(content.trim()) {
            return calls;
        }

        // Auto-detection of Aider-style Search/Replace blocks:
        // If LLM outputs <<<<<<< SEARCH blocks with file paths without strict JSON,
        // parse and convert them to search_replace ToolCalls.
        let sr_blocks = crate::search_replace::parse_search_replace_blocks(content);
        if !sr_blocks.is_empty() {
            let mut calls = Vec::new();
            for block in sr_blocks {
                if let Some(path) = block.file_path {
                    calls.push(ToolCall {
                        id: String::new(),
                        tool: "search_replace".to_string(),
                        args: serde_json::json!({
                            "path": path,
                            "search": block.search,
                            "replace": block.replace,
                        }),
                    });
                }
            }
            if !calls.is_empty() {
                return calls;
            }
        }

        Vec::new()
    }


    /// Synchronous legacy entry point (tests, scripts): runs one tool call
    /// with a fresh yolo context rooted at the current directory.
    pub fn exec_tool(&self, call: &ToolCall) -> Result<String> {
        let ctx = ToolContext::new(Workspace::current_dir()?, PermissionMode::Yolo);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(tools::execute(&ctx, call))
    }

    // --------------------------------------------------------------- turn ----

    fn tool_calls_from(&self, resp: &LlmResponse, mode: ToolMode, step: usize) -> Vec<ToolCall> {
        let mut calls = resp.tool_calls.clone();
        if calls.is_empty() && (mode == ToolMode::Text || resp.content.contains("```json") || resp.content.contains("<<<<<<< SEARCH")) {
            calls = self.parse_tool_calls(&resp.content);
        }
        for (i, c) in calls.iter_mut().enumerate() {
            if c.id.is_empty() {
                c.id = format!("call_{}_{}", step + 1, i + 1);
            }
        }
        calls
    }

    async fn run_one_tool(ctx: &ToolContext, call: &ToolCall) -> (String, bool) {
        if call.args.get("__invalid_json__").is_some() {
            return (format!("Error: arguments for `{}` were not valid JSON ({}). Re-send the call with valid JSON arguments.", call.tool, call.args["__parse_error__"]), true);
        }
        match tools::execute(ctx, call).await {
            Ok(out) => (out, false),
            Err(e) => (format!("Error: {}", e), true),
        }
    }

    /// Core loop shared by the TUI worker, the REPL and subagents.
    pub async fn run_loop(
        &mut self,
        mut steering_rx: Option<mpsc::UnboundedReceiver<String>>,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        let mut ctx = match self.ensure_tool_ctx() {
            Ok(c) => c,
            Err(e) => return TurnOutcome::Failed(format!("workspace error: {}", e)),
        };
        ctx.events = self.event_tx.clone();
        ctx.cancel = cancel.clone();
        if !self.is_subagent {
            ctx.subagents = Some(Arc::new(crate::subagent::SubagentRuntime::new(self, self.subagent_limit)));
        }
        self.tool_ctx = Some(ctx.clone());
        if !self.extensions_announced && !self.is_subagent {
            self.extensions_announced = true;
            if let Some(m) = &ctx.mcp {
                for line in m.ensure_started().await {
                    self.emit(AgentEvent::Thinking(line));
                }
            }
            for e in &ctx.hooks.errors {
                self.emit(AgentEvent::Thinking(format!("hooks: {}", e)));
            }
            if !ctx.trusted {
                let ignored = crate::trust::project_exec_files(ctx.workspace.root());
                if !ignored.is_empty() {
                    self.emit(AgentEvent::Thinking(format!("Ignoring project {} — workspace not trusted (run /trust or --trust-workspace)", ignored.join(", "))));
                }
            }
        } else if let Some(m) = &ctx.mcp {
            m.ensure_started().await;
        }
        let turn_start_tokens = self.total_usage.total_tokens;
        let mut last_sig = String::new();
        let mut repeats = 0usize;

        for step in 0..self.max_steps.max(1) {
            if cancel.is_cancelled() {
                return TurnOutcome::Cancelled;
            }
            if let Some(rx) = steering_rx.as_mut() {
                while let Ok(steer) = rx.try_recv() {
                    self.history.push(Message::user(format!("[Steering / mid-turn instruction from the user] {}", steer)));
                }
            }
            self.emit(AgentEvent::Thinking(format!("ReAct step {} / {}", step + 1, self.max_steps)));

            // auto-compact before the request would overflow
            let repo = self.repo_context(&ctx);
            let extra = if repo.is_empty() { String::new() } else { format!("\n\n{}", repo) };
            let sys_est = estimate_tokens(&extra) + 2_000;
            if (self.estimate_prompt_tokens("") + sys_est) as f64 > self.context_limit() as f64 * AUTO_COMPACT_RATIO {
                match self.compact(true).await {
                    Ok(Some(msg)) => self.emit(AgentEvent::Compacted(msg)),
                    Ok(None) => {}
                    Err(e) => self.emit(AgentEvent::Thinking(format!("auto-compact failed: {}", e))),
                }
            }

            let mut overflow_retry = false;
            let (resp, prov, mode) = loop {
                let res = tokio::select! {
                    _ = cancel.cancelled() => return TurnOutcome::Cancelled,
                    r = self.call_model(&extra, true, Some(&cancel)) => r,
                };
                match res {
                    Ok(v) => break v,
                    Err(LlmError::ContextOverflow(m)) if !overflow_retry => {
                        overflow_retry = true;
                        self.emit(AgentEvent::Thinking(format!("context overflow reported by provider ({}); compacting", crate::text::truncate_chars(&m, 80))));
                        match self.compact(true).await {
                            Ok(Some(msg)) => self.emit(AgentEvent::Compacted(msg)),
                            _ => return TurnOutcome::Failed(format!("context window exceeded and compaction could not help: {}", m)),
                        }
                    }
                    Err(e) => {
                        if cancel.is_cancelled() {
                            return TurnOutcome::Cancelled;
                        }
                        return TurnOutcome::Failed(format!("All providers failed (primary + {} fallbacks). Last error: {}", self.fallback_chain.len(), e));
                    }
                }
            };
            self.account(&prov, &resp.usage);

            let calls = self.tool_calls_from(&resp, mode, step);
            let mut assistant = Message::assistant(resp.content.clone());
            assistant.tool_calls = calls.clone();
            self.history.push(assistant);
            self.emit(AgentEvent::AssistantChunk(resp.content.clone()));

            if calls.is_empty() {
                if resp.stop_reason.as_deref() == Some("length") || resp.stop_reason.as_deref() == Some("max_tokens") {
                    self.emit(AgentEvent::Thinking("model hit max_tokens; answer may be cut off".into()));
                }
                return TurnOutcome::Finished;
            }

            // loop guard: identical tool calls 3 steps in a row
            let sig = calls.iter().map(|c| format!("{}:{}", c.tool, c.args)).collect::<Vec<_>>().join("|");
            if sig == last_sig {
                repeats += 1;
            } else {
                repeats = 0;
                last_sig = sig;
            }

            let all_tasks = calls.len() > 1 && calls.iter().all(|c| c.tool == "task");
            if all_tasks {
                for c in &calls {
                    self.emit(AgentEvent::ToolStart { tool: c.tool.clone(), args: c.args.to_string() });
                }
                let futs = calls.iter().map(|c| {
                    let ctx = ctx.clone();
                    let c = c.clone();
                    async move { (c.clone(), Self::run_one_tool(&ctx, &c).await) }
                });
                let results = tokio::select! {
                    r = futures_util::future::join_all(futs) => r,
                    _ = cancel.cancelled() => {
                        for c in &calls {
                            self.history.push(Message::tool_result(c, "Cancelled by user", true));
                        }
                        return TurnOutcome::Cancelled;
                    }
                };
                for (c, (out, is_err)) in results {
                    self.emit(AgentEvent::ToolOutput { tool: c.tool.clone(), output: out.clone(), is_error: is_err });
                    self.history.push(Message::tool_result(&c, out, is_err));
                }
            } else {
                for (i, c) in calls.iter().enumerate() {
                    self.emit(AgentEvent::ToolStart { tool: c.tool.clone(), args: c.args.to_string() });
                    let (out, is_err) = if repeats >= 2 {
                        (format!("Error: this exact `{}` call was already made {} times with the same arguments. Change approach.", c.tool, repeats + 1), true)
                    } else {
                        tokio::select! {
                            r = Self::run_one_tool(&ctx, c) => r,
                            _ = cancel.cancelled() => {
                                for rest in &calls[i..] {
                                    self.history.push(Message::tool_result(rest, "Cancelled by user before completion", true));
                                }
                                return TurnOutcome::Cancelled;
                            }
                        }
                    };
                    self.emit(AgentEvent::ToolOutput { tool: c.tool.clone(), output: out.clone(), is_error: is_err });
                    self.history.push(Message::tool_result(c, out, is_err));
                }
            }
            let live_mode = ctx.mode();
            if live_mode != self.permission_mode {
                self.permission_mode = live_mode;
            }
            if repeats >= 3 {
                self.history.push(Message::assistant("[Stopped: the same tool call repeated without progress.]"));
                return TurnOutcome::LoopDetected;
            }
            if self.turn_token_budget > 0 && self.total_usage.total_tokens - turn_start_tokens > self.turn_token_budget {
                return TurnOutcome::TokenBudget;
            }
        }
        TurnOutcome::StepBudget
    }

    /// Drop every clone of the event sender (agent, tool context, subagent
    /// runtime) so a front-end's `rx.recv()` loop can terminate.
    pub fn detach_events(&mut self) {
        self.event_tx = None;
        if let Some(ctx) = self.tool_ctx.as_mut() {
            ctx.events = None;
            ctx.subagents = None;
        }
    }

    fn sync_event(&self) -> AgentEvent {
        AgentEvent::HistorySync {
            history: self.history.clone(),
            total_usage: self.total_usage.clone(),
            last_usage: self.last_usage.clone(),
            total_calls: self.total_calls,
        }
    }

    /// Non-blocking ReAct turn worker for front-ends. Streams events through
    /// `events`, accepts mid-turn steering through `steering_rx`, aborted by `cancel`.
    pub async fn run_turn(
        &mut self,
        prompt: String,
        events: AgentEvents,
        steering_rx: mpsc::UnboundedReceiver<String>,
        cancel: CancellationToken,
    ) {
        let started = Instant::now();
        self.event_tx = Some(events.clone());
        self.add_user(&prompt);
        let turn_usage_start = self.total_usage.clone();
        let outcome = self.run_loop(Some(steering_rx), cancel).await;
        if self.autosave {
            let _ = self.save_session(&crate::sessions::SessionStore::open_default());
        }
        let mut turn_usage = self.total_usage.clone();
        turn_usage.prompt_tokens -= turn_usage_start.prompt_tokens;
        turn_usage.completion_tokens -= turn_usage_start.completion_tokens;
        turn_usage.total_tokens -= turn_usage_start.total_tokens;
        match outcome {
            TurnOutcome::Cancelled => {
                let _ = events.send(self.sync_event());
                let _ = events.send(AgentEvent::Cancelled);
            }
            TurnOutcome::Failed(e) => {
                let _ = events.send(self.sync_event());
                let _ = events.send(AgentEvent::Error(e));
            }
            other => {
                if other != TurnOutcome::Finished {
                    let note = match other {
                        TurnOutcome::StepBudget => format!("⏸ Reached the step budget ({} ReAct steps). Send 'lanjut' / 'continue' to keep going.", self.max_steps),
                        TurnOutcome::LoopDetected => "⏸ Stopped: the agent repeated the same tool call without progress.".into(),
                        TurnOutcome::TokenBudget => "⏸ Stopped: token budget for this turn exhausted.".into(),
                        _ => String::new(),
                    };
                    let _ = events.send(AgentEvent::AssistantChunk(note));
                }
                let _ = events.send(AgentEvent::TurnFinished {
                    usage: turn_usage,
                    total_calls: self.total_calls,
                    duration_ms: started.elapsed().as_millis() as u64,
                    history: self.history.clone(),
                    total_usage: self.total_usage.clone(),
                    last_usage: self.last_usage.clone(),
                });
            }
        }
        self.detach_events();
    }
}

// ------------------------------------------------------------------ REPL ----

fn print_event(ev: &AgentEvent, streamed: &mut bool) {
    match ev {
        AgentEvent::Thinking(s) => eprintln!("\x1b[2m· {}\x1b[0m", s),
        AgentEvent::AssistantDelta(d) => {
            *streamed = true;
            print!("{}", d);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        AgentEvent::AssistantChunk(c) => {
            if !*streamed && !c.is_empty() {
                println!("{}", c);
            } else if *streamed {
                println!();
            }
            *streamed = false;
        }
        AgentEvent::ToolStart { tool, args } => println!("\x1b[36m⚙ {}\x1b[0m {}", tool, crate::text::truncate_chars(args, 160)),
        AgentEvent::ToolOutput { output, is_error, .. } => {
            let color = if *is_error { "31" } else { "2" };
            println!("\x1b[{}m{}\x1b[0m", color, clamp_output(output, 2_000, 25));
        }
        AgentEvent::Compacted(m) => println!("\x1b[33m🗜 {}\x1b[0m", m),
        AgentEvent::Subagent { id, description, status } => println!("\x1b[35m⑂ subagent #{} {} — {:?}\x1b[0m", id, description, status),
        AgentEvent::Error(e) => eprintln!("\x1b[31mError: {}\x1b[0m", e),
        AgentEvent::Cancelled => println!("[cancelled]"),
        AgentEvent::TurnFinished { usage, duration_ms, .. } => {
            println!("\x1b[2m— {} tokens{} · {:.1}s\x1b[0m", usage.total_tokens, if usage.estimated { " (est.)" } else { "" }, *duration_ms as f64 / 1000.0)
        }
        AgentEvent::Todos(t) => {
            for e in t {
                let mark = match e.status.as_str() { "completed" => "☑", "in_progress" => "▸", _ => "☐" };
                println!("\x1b[2m{} {}\x1b[0m", mark, e.content);
            }
        }
        AgentEvent::ModeChanged(m) => println!("\x1b[33m🔐 permission mode → {}\x1b[0m", m.label()),
        AgentEvent::ApprovalRequired(_) | AgentEvent::HistorySync { .. } => {}
    }
}

fn ask_approval_stdin(t: &crate::events::ApprovalTicket) -> ApprovalDecision {
    use std::io::{BufRead, Write};
    println!("\n\x1b[1;33m⚠ Approval needed:\x1b[0m {}", t.request.summary);
    if let Some(d) = &t.request.diff {
        for l in clamp_output(d, 12_000, 200).lines() {
            let c = if l.starts_with('+') && !l.starts_with("+++") { "32" } else if l.starts_with('-') && !l.starts_with("---") { "31" } else if l.starts_with("@@") { "36" } else { "0" };
            println!("\x1b[{}m{}\x1b[0m", c, l);
        }
    }
    print!("[y] yes  [a] always (this session)  [n] no  › ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" | "ya" => ApprovalDecision::Approve,
        "a" | "always" => ApprovalDecision::ApproveAlways,
        other => ApprovalDecision::Reject(if other.is_empty() || other == "n" || other == "no" { None } else { Some(other.to_string()) }),
    }
}

pub async fn run() -> Result<()> {
    use reedline::{DefaultPrompt, FileBackedHistory, Reedline, Signal};
    println!("CACode {} — Sovereign ReAct Agent REPL", CA_VERSION);
    println!("Type /help for commands, Ctrl+D to quit.\n");

    let cfg = crate::config::CaCodeConfig::load().unwrap_or_default();
    let mut agent = ReActAgent::with_credentials(Credentials::from_config(&cfg));
    if let Some(m) = PermissionMode::parse(&cfg.permission_mode) {
        agent.permission_mode = m;
    }
    agent.subagent_limit = if cfg.is_pro() { 49 } else { 5 };
    agent.sandbox = cfg.sandbox_config();
    agent.web_allow_domains = cfg.web_allow_domains.clone();
    let hist_path = dirs::home_dir().unwrap_or_default().join(".cacode").join("repl_history.txt");
    let _ = std::fs::create_dir_all(hist_path.parent().unwrap_or(std::path::Path::new(".")));
    let mut line_editor = Reedline::create();
    if let Ok(h) = FileBackedHistory::with_file(5_000, hist_path) {
        line_editor = line_editor.with_history(Box::new(h));
    }
    let prompt = DefaultPrompt::default();

    loop {
        let sig = line_editor.read_line(&prompt);
        let buffer = match sig {
            Ok(Signal::Success(b)) => b,
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(err) => {
                eprintln!("REPL Error: {}", err);
                break;
            }
        };
        let input: String = buffer.trim().to_string();
        let input = input.as_str();
        if input.is_empty() {
            continue;
        }
        match input.split_whitespace().next().unwrap_or("") {
            "/help" => {
                println!("/clear  /compress  /undo  /mode <ask|auto-edit|yolo|read-only>  /exit");
                continue;
            }
            "/clear" => {
                agent.history.clear();
                println!("Session context cleared.");
                continue;
            }
            "/exit" | "/quit" => break,
            "/compress" => {
                match agent.compress_history().await {
                    Ok(m) => println!("{}", m),
                    Err(e) => eprintln!("Compression failed: {}", e),
                }
                continue;
            }
            "/undo" => {
                match agent.ensure_tool_ctx().and_then(|c| c.undo.undo_last()) {
                    Ok(m) => println!("{}", m),
                    Err(e) => eprintln!("{}", e),
                }
                continue;
            }
            "/mode" => {
                match input.split_whitespace().nth(1).and_then(PermissionMode::parse) {
                    Some(m) => {
                        agent.permission_mode = m;
                        println!("Permission mode: {}", m.label());
                    }
                    None => println!("Permission mode: {} (usage: /mode ask|auto-edit|yolo|read-only)", agent.permission_mode.label()),
                }
                continue;
            }
            _ => {}
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_steer_tx, steer_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        let ctrlc = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                c2.cancel();
            }
        });
        let mut worker = agent.clone();
        let owned = input_owned(input);
        let handle = tokio::spawn(async move {
            worker.run_turn(owned, tx, steer_rx, cancel).await;
            worker.detach_events();
            worker
        });
        let mut streamed = false;
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::ApprovalRequired(t) = &ev {
                let t = t.clone();
                let d = tokio::task::spawn_blocking(move || {
                    let d = ask_approval_stdin(&t);
                    t.respond(d.clone());
                    d
                })
                .await;
                if d.is_err() {
                    break;
                }
                continue;
            }
            print_event(&ev, &mut streamed);
        }
        ctrlc.abort();
        if let Ok(w) = handle.await {
            agent = w;
        }
    }
    println!("Exiting CACode REPL.");
    Ok(())
}

fn input_owned(s: &str) -> String {
    s.to_string()
}

#[cfg(test)]
mod tests;
