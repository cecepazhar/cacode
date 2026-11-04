use anyhow::Result;
use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Row, Table, Wrap},
    Frame, Terminal,
};
use rusqlite::Connection;
use std::fs;
use std::io;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use cacode_core::commands::chat::{AgentEvent, Message, Provider, ReActAgent, TokenUsage, CA_VERSION};
use cacode_core::events::{ApprovalDecision, ApprovalTicket, SubagentStatus};
use cacode_core::llm::Credentials;
use cacode_core::permission::PermissionMode;
use cacode_core::text::truncate_chars;
use crate::config::{CaCodeConfig, CustomProviderConfig};

/// Theme objects come from CATUI (shared CA Family TUI kit). Legacy names
/// (`classic`, `tokyo`, `nord`, `onedark`, `contrast`, `light`) still resolve.
pub use catui::{Theme, ThemePreset};

/// Resolve a theme for CACode; unknown names fall back to `midnight`
/// (CACode's historical "classic" look).
pub fn cacode_theme(name: &str) -> Theme {
    Theme::from_name_or(name, ThemePreset::Midnight).for_app("cacode").adapted()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WizardState {
    None,
    PrdStep1ProjectName,
    PrdStep2Stack { project_name: String },
    PrdStep3Structure { project_name: String, stack: String },
}

pub fn resolve_provider_string(name: &str, config: &CaCodeConfig) -> Option<Provider> {
    let lower = name.trim().to_lowercase();
    match lower.as_str() {
        "openai" => Some(Provider::OpenAI {
            model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into()),
        }),
        "anthropic" => Some(Provider::Anthropic {
            model: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-20240620".into()),
        }),
        "ollama" => {
            let model = config.ollama_model.clone()
                .or_else(|| std::env::var("OLLAMA_MODEL").ok())
                .unwrap_or_else(|| "llama3".into());
            Some(Provider::Ollama { model })
        }
        "9router" | "router9" => {
            let model = config.router9_model.clone()
                .or_else(|| std::env::var("ROUTER9_MODEL").ok())
                .unwrap_or_else(|| "ZA126_PRO".into());
            Some(Provider::Router9 { model })
        }
        "antigravity" | "antigravityzen" | "zen" => {
            let model = config.effective_antigravity_model();
            Some(Provider::AntigravityZen { model })
        }
        custom_name => {
            config.custom_providers.iter().find(|cp| cp.id.eq_ignore_ascii_case(custom_name) || cp.name.eq_ignore_ascii_case(custom_name)).map(|cp| {
                Provider::Custom {
                    name: cp.name.clone(),
                    base_url: cp.base_url.clone(),
                    api_key: cp.api_key.clone(),
                    model: cp.model.clone(),
                }
            })
        }
    }
}

pub fn build_fallback_chain(config: &CaCodeConfig) -> Vec<Provider> {
    let mut chain = Vec::new();
    if let Some(ref fb1) = config.fallback_1 {
        if let Some(p) = resolve_provider_string(fb1, config) {
            chain.push(p);
        }
    }
    if let Some(ref fb2) = config.fallback_2 {
        if let Some(p) = resolve_provider_string(fb2, config) {
            chain.push(p);
        }
    }
    if let Some(ref fb3) = config.fallback_3 {
        if let Some(p) = resolve_provider_string(fb3, config) {
            chain.push(p);
        }
    }
    if chain.is_empty() {
        chain.push(Provider::Router9 { model: "ZA126_PRO".into() });
        chain.push(Provider::Ollama { model: "llama3".into() });
    }
    chain
}


const LOGO: &[&str] = &[
    r"  ██████╗ █████╗  ██████╗ ██████╗ ██████╗ ███████╗",
    r" ██╔════╝██╔══██╗██╔════╝██╔═══██╗██╔══██╗██╔════╝",
    r" ██║     ███████║██║     ██║   ██║██║  ██║█████╗  ",
    r" ██║     ██╔══██║██║     ██║   ██║██║  ██║██╔══╝  ",
    r" ╚██████╗██║  ██║╚██████╗╚██████╔╝██████╔╝███████╗",
    r"  ╚═════╝╚═╝  ╚═╝ ╚═════╝ ╚═════╝ ╚═════╝ ╚══════╝",
];

const WELCOME_LINES: &[&str] = &[
    "",
    "  Sovereign Rust Development Agent  v1.0.9",
    "",
    "  • ReAct Agent Loop with deterministic local tool execution",
    "  • Multi-provider BYOK & Free: Antigravity Zen, Ollama, 9Router, OpenAI, Anthropic",
    "  • Zero telemetry — your keys, your machine, your data",
];

const PAGE_NAMES: &[&str] = &["Chat", "Monitor", "Report", "Skills", "Settings"];
const MAX_SESSIONS: usize = 9;

/// Generate a concise default name for a new session: "Term N".
pub fn default_session_name(idx: usize) -> String {
    format!("Term {}", idx + 1)
}

/// Truncate a session name to <=12 chars, preserving numeric suffix.
pub fn short_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 12 {
        name.to_string()
    } else {
        let truncated: String = chars.iter().take(12).collect();
        // Preserve any trailing digits so "Term 10" -> "Term 10" stays readable
        truncated
    }
}


const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/models", "Interactive AI model picker"),
    ("/providers", "Probe & display local & free providers (Ollama, Zen)"),
    ("/provider", "Manage & switch AI providers (/provider add|list|<name>)"),
    ("/fallback", "Configure fallback routing tier (/fallback <1|2|3> <provider>)"),
    ("/theme", "Switch UI theme (/theme <classic|tokyo|nord|onedark|contrast|light>)"),
    ("/pro", "Pro edition differentiation & license activation (/pro <key>)"),
    ("/mode", "Select thinking level: Fast, Analytical, Deep"),
    ("/settings", "Open BYOK settings & provider configuration"),
    ("/monitor", "View real-time token telemetry & proxy log"),
    ("/report", "View token usage summary, heatmap & model analytics"),
    ("/skills", "Manage custom skills & reusable prompts"),
    ("/todo", "Manage task checklist (/todo add|done|list|clear)"),
    ("/dev", "⚡ Feature development & implementation protocol (@dev)"),
    ("/prd", "📋 Product Requirements Document drafting wizard (@prd)"),
    ("/audit", "🛡️ Security & architecture whitebox audit (@audit)"),
    ("/refact", "🧹 Code refactoring, simplification & cleanup"),
    ("/update", "📦 Dependency bump, migration & semantic version update"),
    ("/fix", "🐞 Systematic 4-phase bugfix protocol (@fix)"),
    ("/bughunt", "🎯 Deep bug hunt (boundary fuzzing, concurrency & panics)"),
    ("/test", "🧪 TDD test authoring (unit & integration tests)"),
    ("/qa", "✅ Quality Assurance dual-check sign-off"),
    ("/compress", "🗜️ Compress conversation history (token optimization)"),
    ("/undo", "↶ Revert the agent's last file edit (exact bytes restored)"),
    ("/permissions", "🔐 Permission mode (/permissions ask|auto-edit|yolo|read-only)"),
    ("/jobs", "List background shell jobs started by the agent"),
    ("/resume", "↺ Resume a saved session (/resume [id])"),
    ("/plan", "🗺 Plan mode: investigate read-only, approve a plan before edits"),
    ("/trust", "Trust this workspace (enables project hooks & .mcp.json)"),
    ("/mcp", "Show MCP servers & tools of this session"),
    ("/context", "📊 Show active context window tokens & usage limit"),
    ("/status", "Show active provider, model, tokens & system stats"),
    ("/sessions", "List all active chat sessions"),
    ("/rename", "Rename current chat session (/rename <name>)"),
    ("/clear", "Clear current conversation memory"),
    ("/export", "Export conversation transcript to Markdown file"),
    ("/version", "Show CACode version"),
    ("/reset", "Soft reset agent context window"),
    ("/cd", "Change working directory (/cd <path>)"),
    ("/pwd", "Print current working directory"),
    ("/help", "Show all available slash commands"),
    ("/exit", "Exit CACode"),
];

const THINKING_MODES: &[(&str, &str)] = &[
    ("Fast", "⚡ Standard direct execution — lowest latency"),
    ("Analytical", "🔍 Multi-step analytical reasoning & verification"),
    ("Deep", "🧠 Deep cognitive reasoning & edge-case proofs"),
];

#[derive(Clone)]
struct ChatMessage {
    role: String,
    content: String,
    /// Per-turn token telemetry attached to assistant messages.
    tokens: Option<TokenUsage>,
    /// Wall-clock duration of the ReAct turn that produced this message.
    duration_ms: Option<u64>,
}

impl ChatMessage {
    fn plain(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tokens: None,
            duration_ms: None,
        }
    }

    #[allow(dead_code)]
    fn with_usage(
        role: impl Into<String>,
        content: impl Into<String>,
        tokens: Option<TokenUsage>,
        duration_ms: Option<u64>,
    ) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tokens,
            duration_ms,
        }
    }
}

/// Formats a number with thousands separators, e.g. `12450` -> `12,450`.
fn fmt_num(n: i64) -> String {
    let neg = n < 0;
    let s = format!("{}", n.abs());
    let mut out = String::new();
    let mut first = true;
    for c in s.chars().rev() {
        if !first && out.len().is_multiple_of(4) {
            out.push(',');
        }
        out.push(c);
        first = false;
    }
    let out = out.chars().rev().collect::<String>();
    if neg {
        format!("-{}", out)
    } else {
        out
    }
}

fn unescape_newlines(s: &str) -> String {
    if s.contains("\\n") || s.contains("\\r\\n") || s.contains("\\t") || s.contains("\\\"") {
        s.replace("\\r\\n", "\n")
            .replace("\\n", "\n")
            .replace("\\t", "    ")
            .replace("\\\"", "\"")
    } else {
        s.to_string()
    }
}

fn format_tool_action(tool: &str, args_raw: &str) -> (String, String) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(args_raw).ok();
    match tool {
        "read_file" => {
            let path = parsed
                .as_ref()
                .and_then(|v| v.get("path").or_else(|| v.get("file")).or_else(|| v.get("filename")))
                .and_then(|v| v.as_str())
                .unwrap_or(args_raw);
            ("Reading file".to_string(), path.to_string())
        }
        "write_file" => {
            let path = parsed
                .as_ref()
                .and_then(|v| v.get("path").or_else(|| v.get("file")))
                .and_then(|v| v.as_str())
                .unwrap_or(args_raw);
            ("Writing file".to_string(), path.to_string())
        }
        "patch_file" | "search_replace" => {
            let path = parsed
                .as_ref()
                .and_then(|v| v.get("path").or_else(|| v.get("file")))
                .and_then(|v| v.as_str())
                .unwrap_or(args_raw);
            ("Patching file".to_string(), path.to_string())
        }
        "search_files" | "grep" => {
            let pat = parsed
                .as_ref()
                .and_then(|v| v.get("pattern").or_else(|| v.get("query")))
                .and_then(|v| v.as_str())
                .unwrap_or(args_raw);
            ("Searching files".to_string(), pat.to_string())
        }
        "exec_command" | "bash" | "sh" | "terminal" => {
            let cmd = parsed
                .as_ref()
                .and_then(|v| v.get("command").or_else(|| v.get("cmd")))
                .and_then(|v| v.as_str())
                .unwrap_or(args_raw);
            ("Running command".to_string(), cmd.to_string())
        }
        _ => {
            let short = if args_raw.chars().count() > 60 {
                truncate_chars(args_raw, 60)
            } else {
                args_raw.to_string()
            };
            (format!("Tool: {}", tool), short)
        }
    }
}

/// Session cost from the shared pricing table (model-aware, not a flat rate).
/// Unknown models count as $0 here; `cacode report` flags them as `?price`.
fn est_token_cost(provider: &str, model: &str, prompt: i64, completion: i64) -> f64 {
    cacode_core::pricing::cost_usd(provider, model, prompt, completion).unwrap_or(0.0)
}

struct SettingsField {
    label: &'static str,
    key: &'static str,
    value: String,
    is_secret: bool,
}

struct SettingsState {
    fields: Vec<SettingsField>,
    selected: usize,
    editing: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

pub struct ChatSession {
    name: String,
    messages: Vec<ChatMessage>,
    agent: ReActAgent,
    input: String,
    cursor_pos: usize,
    scroll_offset: u16,
    total_tokens: u64,
    show_welcome: bool,
    pub react_steps: Vec<String>,
    pub todos: Vec<TodoItem>,
    pub wizard_state: WizardState,
    /// In-flight ReAct turn of THIS session (each tab runs independently).
    turn: Option<TurnRuntime>,
    /// Pending approval requests raised by this session's agent.
    approvals: VecDeque<ApprovalTicket>,
    /// True while the last assistant message is being streamed into.
    streaming: bool,
    /// Large pastes folded into `[Pasted #n …]` placeholders.
    pastes: Vec<String>,
}

/// Runtime handles of one background turn.
struct TurnRuntime {
    rx: mpsc::UnboundedReceiver<AgentEvent>,
    steer_tx: mpsc::UnboundedSender<String>,
    cancel: CancellationToken,
    started: Instant,
    active_tool: Option<(String, Instant)>,
    streamed_chars: usize,
}

/// Apply config to an agent (credentials, persona, routing, permissions).
pub fn configure_agent(agent: &mut ReActAgent, config: &CaCodeConfig) {
    agent.creds = Credentials::from_config(config);
    agent.user_name = config.user_name.clone();
    agent.persona = config.system_persona.clone();
    agent.thinking_mode = config.thinking_mode.clone();
    agent.fallback_chain = build_fallback_chain(config);
    agent.max_retries = config.max_retries.max(1);
    if let Some(m) = PermissionMode::parse(&config.permission_mode) {
        agent.permission_mode = m;
    }
    agent.subagent_limit = if config.is_pro() { 49 } else { 5 };
    agent.max_steps = config.max_steps.max(1);
    agent.context_window_override = (config.context_window > 0).then_some(config.context_window);
    agent.sandbox = config.sandbox_config();
    agent.web_allow_domains = config.web_allow_domains.clone();
}

impl ChatSession {
    fn new(name: String, config: &CaCodeConfig) -> Self {
        let creds = Credentials::from_config(config);
        let mut agent = ReActAgent::with_credentials(creds);
        configure_agent(&mut agent, config);
        Self {
            name,
            messages: Vec::new(),
            agent,
            input: String::new(),
            cursor_pos: 0,
            scroll_offset: 0,
            total_tokens: 0,
            show_welcome: true,
            react_steps: Vec::new(),
            todos: Vec::new(),
            wizard_state: WizardState::None,
            turn: None,
            approvals: VecDeque::new(),
            streaming: false,
            pastes: Vec::new(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.turn.is_some()
    }

    /// Expand `[Pasted #n …]` placeholders back to their content.
    fn expand_pastes(&self, input: &str) -> String {
        let mut out = input.to_string();
        for (i, p) in self.pastes.iter().enumerate() {
            let tag = paste_tag(i + 1, p);
            out = out.replace(&tag, p);
        }
        out
    }

    pub fn update_todos_from_text(&mut self, text: &str) {
        for line in text.lines() {
            let trimmed = line.trim();
            let rest = if let Some(r) = trimmed.strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| trimmed.strip_prefix("+ "))
            {
                r.trim()
            } else if let Some(dot_idx) = trimmed.find(". ") {
                if trimmed[..dot_idx].chars().all(|c| c.is_ascii_digit()) {
                    trimmed[dot_idx + 2..].trim()
                } else {
                    trimmed
                }
            } else {
                trimmed
            };

            let (is_todo, done, todo_text) = if let Some(item_text) = rest.strip_prefix("[ ] ") {
                (true, false, item_text.trim())
            } else if let Some(item_text) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
                (true, true, item_text.trim())
            } else if rest.starts_with("[ ]") {
                (true, false, rest.trim_start_matches("[ ]").trim())
            } else if rest.starts_with("[x]") || rest.starts_with("[X]") {
                (true, true, rest[3..].trim())
            } else {
                (false, false, "")
            };

            if is_todo && !todo_text.is_empty() {
                if let Some(existing) = self.todos.iter_mut().find(|t| t.text == todo_text) {
                    existing.done = done;
                } else {
                    self.todos.push(TodoItem {
                        text: todo_text.to_string(),
                        done,
                    });
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn sync_todos_from_steps(&mut self) {
        if self.todos.is_empty() {
            for step in &self.react_steps {
                let clean = step.trim();
                let (done, text) = if let Some(s) = clean.strip_prefix("✓ ") {
                    (true, s.trim())
                } else if let Some(s) = clean.strip_prefix("● ") {
                    (false, s.trim())
                } else if let Some(s) = clean.strip_prefix("✗ ") {
                    (false, s.trim())
                } else {
                    (false, clean)
                };
                if !text.is_empty() {
                    self.todos.push(TodoItem {
                        text: text.to_string(),
                        done,
                    });
                }
            }
        }
    }
}

pub fn get_cursor_coords(input: &str, cursor_pos: usize) -> (usize, usize) {
    let mut current_pos = 0;
    let lines: Vec<&str> = input.split('\n').collect();
    for (row, line) in lines.iter().enumerate() {
        let line_len = line.len();
        if cursor_pos <= current_pos + line_len {
            let offset_bytes = cursor_pos.saturating_sub(current_pos);
            let sub = if offset_bytes >= line_len {
                *line
            } else if line.is_char_boundary(offset_bytes) {
                &line[..offset_bytes]
            } else {
                let safe_end = line
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|&i| i < offset_bytes)
                    .last()
                    .unwrap_or(0);
                &line[..safe_end]
            };
            let col = sub.chars().count();
            return (row, col);
        }
        current_pos += line_len + 1;
    }
    let last_row = lines.len().saturating_sub(1);
    let last_col = lines.last().map(|l| l.chars().count()).unwrap_or(0);
    (last_row, last_col)
}

pub fn detect_image_reference(text: &str) -> Option<String> {
    for word in text.split_whitespace() {
        if let Some(rest) = word.strip_prefix("@img:").or_else(|| word.strip_prefix("@image:")) {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    if let Some(start) = text.find("![") {
        if let Some(mid) = text[start..].find("](") {
            let after_mid = start + mid + 2;
            if let Some(end) = text[after_mid..].find(')') {
                let target = &text[after_mid..after_mid + end];
                return Some(target.to_string());
            }
        }
    }
    const IMG_EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif", ".svg"];
    for word in text.split_whitespace() {
        let clean = word.trim_matches(|c| c == '(' || c == ')' || c == '"' || c == '\'' || c == '<' || c == '>');
        let lower = clean.to_lowercase();
        for ext in IMG_EXTS {
            if lower.ends_with(ext) {
                return Some(clean.to_string());
            }
        }
    }
    None
}

pub fn render_styled_line(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    let len = text.len();

    const IMG_EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif", ".svg"];

    fn extract_filename(target: &str) -> String {
        let clean = target.trim_matches(|c| c == '(' || c == ')' || c == '"' || c == '\'' || c == '<' || c == '>');
        clean.rsplit('/').next().unwrap_or(clean).to_string()
    }

    while cursor < len {
        if text[cursor..].starts_with("![") {
            if let Some(mid) = text[cursor..].find("](") {
                let url_start = cursor + mid + 2;
                if let Some(end) = text[url_start..].find(')') {
                    let url = &text[url_start..url_start + end];
                    let filename = extract_filename(url);
                    spans.push(Span::styled(
                        format!("[🖼️ Image: {}]", filename),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ));
                    cursor = url_start + end + 1;
                    continue;
                }
            }
        }

        let next_space = text[cursor..].find(char::is_whitespace).map(|i| cursor + i).unwrap_or(len);
        let token = &text[cursor..next_space];

        if let Some(rest) = token.strip_prefix("@img:").or_else(|| token.strip_prefix("@image:")) {
            let filename = extract_filename(rest);
            spans.push(Span::styled(
                format!("[🖼️ Image: {}]", filename),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
            cursor = next_space;
            let ws_end = text[cursor..].find(|c: char| !c.is_whitespace()).map(|i| cursor + i).unwrap_or(len);
            if ws_end > cursor {
                spans.push(Span::styled(text[cursor..ws_end].to_string(), base_style));
                cursor = ws_end;
            }
            continue;
        }

        let clean = token.trim_matches(|c| c == '(' || c == ')' || c == '"' || c == '\'' || c == '<' || c == '>' || c == ',' || c == '.');
        let lower = clean.to_lowercase();
        let is_img = IMG_EXTS.iter().any(|ext| lower.ends_with(ext));

        if is_img && !clean.is_empty() {
            let pfx_len = token.find(clean).unwrap_or(0);
            if pfx_len > 0 {
                spans.push(Span::styled(token[..pfx_len].to_string(), base_style));
            }
            let filename = extract_filename(clean);
            spans.push(Span::styled(
                format!("[🖼️ Image: {}]", filename),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
            let sfx_start = pfx_len + clean.len();
            if sfx_start < token.len() {
                spans.push(Span::styled(token[sfx_start..].to_string(), base_style));
            }
        } else if clean.starts_with("http://") || clean.starts_with("https://") || clean.starts_with("git@") {
            let pfx_len = token.find(clean).unwrap_or(0);
            if pfx_len > 0 {
                spans.push(Span::styled(token[..pfx_len].to_string(), base_style));
            }
            spans.push(Span::styled(
                clean.to_string(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED),
            ));
            let sfx_start = pfx_len + clean.len();
            if sfx_start < token.len() {
                spans.push(Span::styled(token[sfx_start..].to_string(), base_style));
            }
        } else {
            spans.push(Span::styled(token.to_string(), base_style));
        }

        cursor = next_space;
        let ws_end = text[cursor..].find(|c: char| !c.is_whitespace()).map(|i| cursor + i).unwrap_or(len);
        if ws_end > cursor {
            spans.push(Span::styled(text[cursor..ws_end].to_string(), base_style));
            cursor = ws_end;
        }
    }

    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base_style));
    }

    spans
}

struct UsageRow {
    timestamp: String,
    model: String,
    total_tokens: i64,
    est_cost: f64,
}

struct ReportRow {
    model: String,
    calls: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    est_cost: f64,
}

struct DailyTrafficRow {
    day: String,
    calls: i64,
    tokens: i64,
    cost: f64,
    bar: String,
}

#[derive(Clone)]
struct SkillItem {
    name: String,
    created: String,
    description: String,
    content: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ModalPicker {
    Off,
    Models,
    Thinking,
}

struct App {
    active_page: usize, // 0=Chat, 1=Monitor, 2=Report, 3=Skills, 4=Settings
    sessions: Vec<ChatSession>,
    active_session: usize,
    config: CaCodeConfig,
    settings: SettingsState,
    status_msg: String,
    is_thinking: bool,
    should_quit: bool,
    pub show_exit_confirmation: bool,
    usage_rows: Vec<UsageRow>,
    report_rows: Vec<ReportRow>,
    daily_rows: Vec<DailyTrafficRow>,
    skills: Vec<SkillItem>,
    skill_selected: usize,
    monitor_scroll: u16,
    report_scroll: u16,
    spinner_frame: usize,
    // Host page transition state
    pub host_step: usize,
    pub host_last_tick: Instant,
    // Autocomplete state
    autocomplete_idx: usize,
    // Modal picker state
    modal_picker: ModalPicker,
    modal_idx: usize,
    models_cache: Vec<String>,
    /// Results of background jobs (e.g. /compress) that must not block the UI.
    bg_tx: mpsc::UnboundedSender<BgResult>,
    bg_rx: mpsc::UnboundedReceiver<BgResult>,
    bg_busy: usize,
    /// Prompt history shared by all tabs, persisted to ~/.cacode/history.txt.
    input_history: Vec<String>,
    history_cursor: Option<usize>,
    /// Ctrl+R reverse search query (Some = search mode active).
    history_search: Option<String>,
    approval_scroll: u16,
    file_cache: Option<(Instant, Vec<String>)>,
    pub theme: Theme,
}

/// Messages from background jobs back to the UI thread.
enum BgResult {
    Compressed { session: usize, history: Vec<Message>, total_usage: TokenUsage, result: Result<String, String> },
}

pub fn paste_tag(n: usize, content: &str) -> String {
    format!("[Pasted #{}: {} lines]", n, content.lines().count())
}

pub fn history_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cacode").join("history.txt"))
}

pub fn load_history() -> Vec<String> {
    history_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|t| t.lines().map(|l| l.replace("\\n", "\n")).filter(|l| !l.trim().is_empty()).collect())
        .unwrap_or_default()
}

impl App {
    fn new(config: CaCodeConfig) -> Self {
        let settings = Self::build_settings(&config);
        let session = ChatSession::new(default_session_name(0), &config);
        let theme = cacode_theme(&config.theme);
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let mut app = Self {
            active_page: 0,
            sessions: vec![session],
            active_session: 0,
            config,
            settings,
            status_msg: String::new(),
            is_thinking: false,
            should_quit: false,
            show_exit_confirmation: false,
            usage_rows: Vec::new(),
            report_rows: Vec::new(),
            daily_rows: Vec::new(),
            skills: Vec::new(),
            skill_selected: 0,
            monitor_scroll: 0,
            report_scroll: 0,
            spinner_frame: 0,
            host_step: 0,
            host_last_tick: Instant::now(),
            autocomplete_idx: 0,
            modal_picker: ModalPicker::Off,
            modal_idx: 0,
            models_cache: Vec::new(),
            bg_tx,
            bg_rx,
            bg_busy: 0,
            input_history: load_history(),
            history_cursor: None,
            history_search: None,
            approval_scroll: 0,
            file_cache: None,
            theme,
        };
        app.refresh_skills();
        app.refresh_models_cache();
        app
    }

    pub fn is_pro(&self) -> bool {
        self.config.is_pro()
    }

    fn session(&self) -> &ChatSession {
        &self.sessions[self.active_session]
    }

    fn session_mut(&mut self) -> &mut ChatSession {
        &mut self.sessions[self.active_session]
    }

    // ------------------------------------------------------------------
    // Non-blocking agent runtime: each submitted prompt spawns a background
    // Tokio task; the event loop only polls `agent_events_rx` on each tick
    // so the UI and keyboard never block on LLM calls or tool execution.
    // ------------------------------------------------------------------

    /// Begin a ReAct turn for the active session in a background task.
    /// Returns immediately; progress is delivered through the session's runtime.
    fn start_turn(&mut self, prompt: String) {
        let session_idx = self.active_session;
        if self.sessions[session_idx].is_busy() {
            // Already busy: forward it as a mid-turn steering instruction.
            self.steer(prompt);
            return;
        }
        let config = self.config.clone();
        let session_name = self.sessions[session_idx].name.clone();
        // Create the session's tool context here (not in the worker clone) so
        // read-state, approvals and background jobs persist across turns.
        if let Err(e) = self.sessions[session_idx].agent.ensure_tool_ctx() {
            self.sessions[session_idx].messages.push(ChatMessage::plain("system", format!("Workspace error: {}", e)));
            return;
        }
        // Apply live settings to the worker's copy so the background task
        // matches the UI state.
        let mut agent = self.sessions[session_idx].agent.clone();
        agent.user_name = config.user_name.clone();
        agent.persona = config.system_persona.clone();
        agent.thinking_mode = config.thinking_mode.clone();

        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        let cancel = CancellationToken::new();
        tokio::spawn({
            let cancel = cancel.clone();
            async move {
                agent.run_turn(prompt, ev_tx, steer_rx, cancel).await;
            }
        });
        let s = &mut self.sessions[session_idx];
        s.streaming = false;
        s.turn = Some(TurnRuntime { rx: ev_rx, steer_tx, cancel, started: Instant::now(), active_tool: None, streamed_chars: 0 });
        self.is_thinking = true;
        self.status_msg = format!("{} — reasoning…", session_name);
    }

    /// Forward a mid-run steering instruction to the active session's worker.
    fn steer(&mut self, instruction: String) {
        if let Some(rt) = &self.session().turn {
            let _ = rt.steer_tx.send(instruction);
        }
    }

    /// Cancel the active session's in-flight LLM call / tool execution.
    /// The worker answers with HistorySync + Cancelled, so nothing is lost.
    fn cancel_turn(&mut self) {
        if let Some(rt) = &self.session().turn {
            rt.cancel.cancel();
        }
        for t in self.session_mut().approvals.drain(..) {
            t.respond(ApprovalDecision::Reject(Some("turn cancelled".into())));
        }
    }

    fn cancel_all_turns(&mut self) {
        for s in &mut self.sessions {
            if let Some(rt) = &s.turn {
                rt.cancel.cancel();
            }
            for t in s.approvals.drain(..) {
                t.respond(ApprovalDecision::Reject(Some("CACode is exiting".into())));
            }
        }
    }

    /// Recompute the global busy flag from the active session + background jobs.
    fn sync_thinking(&mut self) {
        self.is_thinking = self.session().is_busy() || self.bg_busy > 0;
    }

    fn drain_bg(&mut self) {
        while let Ok(r) = self.bg_rx.try_recv() {
            self.bg_busy = self.bg_busy.saturating_sub(1);
            match r {
                BgResult::Compressed { session, history, total_usage, result } => {
                    if let Some(s) = self.sessions.get_mut(session) {
                        match result {
                            Ok(msg) => {
                                s.agent.history = history;
                                s.agent.total_usage = total_usage;
                                s.messages.push(ChatMessage::plain("system", msg));
                                self.status_msg = "Context compressed ✓".into();
                            }
                            Err(e) => {
                                s.messages.push(ChatMessage::plain("system", format!("Compression failed: {}", e)));
                                self.status_msg = "Compression failed".into();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Drains pending agent events of every session (non-blocking).
    fn drain_agent_events(&mut self) {
        self.drain_bg();
        for idx in 0..self.sessions.len() {
            let Some(mut rt) = self.sessions[idx].turn.take() else { continue };
            let mut finished = false;
            while let Ok(ev) = rt.rx.try_recv() {
                if self.apply_event(idx, &mut rt, ev) {
                    finished = true;
                }
            }
            if !finished && rt.rx.is_closed() && rt.rx.is_empty() {
                // worker vanished without a terminal event (panic): never stay "busy" forever
                self.sessions[idx].messages.push(ChatMessage::plain("system", "Error: agent worker stopped unexpectedly".to_string()));
                finished = true;
            }
            if !finished {
                self.sessions[idx].turn = Some(rt);
            } else {
                self.sessions[idx].streaming = false;
                if idx == self.active_session {
                    self.status_msg.clear();
                }
            }
        }
        self.sync_thinking();
    }

    /// Apply one event to session `target`. Returns true when the turn ended.
    fn apply_event(&mut self, target: usize, rt: &mut TurnRuntime, ev: AgentEvent) -> bool {
        let is_active = target == self.active_session;
        match ev {
            AgentEvent::Thinking(stage) => {
                self.sessions[target].show_welcome = false;
                let step_label = format!("● {}", stage);
                if self.sessions[target].react_steps.last().map(|s| s.as_str()) != Some(&step_label) {
                    self.sessions[target].react_steps.push(step_label);
                }
                if self.sessions[target].react_steps.len() > 50 {
                    let excess = self.sessions[target].react_steps.len() - 50;
                    self.sessions[target].react_steps.drain(0..excess);
                }
                if is_active {
                    self.status_msg = format!("{} — {}", self.sessions[target].name, stage);
                }
            }
            AgentEvent::AssistantDelta(d) => {
                rt.streamed_chars += d.chars().count();
                let s = &mut self.sessions[target];
                if s.streaming {
                    if let Some(m) = s.messages.last_mut() {
                        m.content.push_str(&d);
                    }
                } else {
                    s.messages.push(ChatMessage::plain("assistant", d));
                    s.streaming = true;
                }
                s.scroll_offset = 0;
            }
            AgentEvent::AssistantChunk(text) => {
                let s = &mut self.sessions[target];
                if s.streaming {
                    if let Some(m) = s.messages.last_mut() {
                        m.content = text.clone();
                        if text.trim().is_empty() {
                            s.messages.pop();
                        }
                    }
                    s.streaming = false;
                } else if !text.trim().is_empty() {
                    s.messages.push(ChatMessage::plain("assistant", text));
                }
                s.scroll_offset = 0;
            }
            AgentEvent::ToolStart { tool, args } => {
                self.sessions[target].streaming = false;
                rt.active_tool = Some((tool.clone(), Instant::now()));
                let (action, target_str) = format_tool_action(&tool, &args);
                let step_item = if target_str.is_empty() { format!("● {}", action) } else { format!("● {} {}", tool, target_str) };
                self.sessions[target].react_steps.push(step_item);
                if self.sessions[target].react_steps.len() > 50 {
                    let excess = self.sessions[target].react_steps.len() - 50;
                    self.sessions[target].react_steps.drain(0..excess);
                }
                let disp_action = if target_str.is_empty() { action } else { format!("{} {}", action, target_str) };
                if self.sessions[target].todos.is_empty() || self.sessions[target].todos.iter().all(|t| t.done) {
                    self.sessions[target].todos.push(TodoItem { text: disp_action.clone(), done: false });
                }
                self.sessions[target].messages.push(ChatMessage::plain("tool_start", format!("● {}", disp_action)));
            }
            AgentEvent::ToolOutput { tool, output, is_error } => {
                rt.active_tool = None;
                let mark = if is_error { "✗" } else { "✓" };
                if let Some(pos) = self.sessions[target].react_steps.iter().rposition(|s| s.starts_with("●") && s.contains(&tool)) {
                    let old = &self.sessions[target].react_steps[pos];
                    let replaced = format!("{} {}", mark, old.trim_start_matches("● ").trim());
                    self.sessions[target].react_steps[pos] = replaced;
                } else if let Some(last) = self.sessions[target].react_steps.last_mut() {
                    if last.starts_with("●") {
                        *last = format!("{} {}", mark, last.trim_start_matches("● ").trim());
                    }
                }
                if !is_error {
                    if let Some(t) = self.sessions[target].todos.iter_mut().rfind(|t| !t.done && (t.text.contains(&tool) || tool.contains(&t.text))) {
                        t.done = true;
                    }
                }
                let clean_out = unescape_newlines(&output);
                let short = if clean_out.chars().count() > 600 { truncate_chars(&clean_out, 600) } else { clean_out };
                let (role, content) = if is_error { ("tool_result_error", format!("[Error: {}]\n{}", tool, short)) } else { ("tool_result", format!("[Result: {}]\n{}", tool, short)) };
                self.sessions[target].messages.push(ChatMessage::plain(role, content));
            }
            AgentEvent::ApprovalRequired(ticket) => {
                let summary = ticket.request.summary.clone();
                self.sessions[target].approvals.push_back(ticket);
                self.approval_scroll = 0;
                self.status_msg = if is_active { format!("⚠ Approval needed: {}", summary) } else { format!("⚠ {} is waiting for approval", self.sessions[target].name) };
            }
            AgentEvent::Subagent { id, description, status } => {
                let line = match status {
                    SubagentStatus::Started => format!("⑂ subagent #{} started: {}", id, description),
                    SubagentStatus::Step(n) => format!("⑂ subagent #{} step {}", id, n),
                    SubagentStatus::Finished { verified } => format!("⑂ subagent #{} finished: {}{}", id, description, if verified { "" } else { " ⚠ unverified" }),
                    SubagentStatus::Failed(e) => format!("⑂ subagent #{} failed: {}", id, e),
                };
                self.sessions[target].react_steps.push(format!("● {}", line));
            }
            AgentEvent::Todos(list) => {
                self.sessions[target].todos = list
                    .iter()
                    .map(|t| TodoItem { text: if t.status == "in_progress" { format!("▸ {}", t.content) } else { t.content.clone() }, done: t.status == "completed" })
                    .collect();
            }
            AgentEvent::ModeChanged(m) => {
                self.sessions[target].agent.permission_mode = m;
                self.sessions[target].messages.push(ChatMessage::plain("system", format!("🔐 Permission mode → {}", m.label())));
            }
            AgentEvent::Compacted(msg) => {
                self.sessions[target].messages.push(ChatMessage::plain("system", format!("🗜 {}", msg)));
            }
            AgentEvent::HistorySync { history, total_usage, last_usage, total_calls } => {
                let s = &mut self.sessions[target];
                s.agent.history = history;
                s.agent.total_usage = total_usage.clone();
                s.agent.last_usage = last_usage;
                s.agent.total_calls = total_calls;
                s.total_tokens = total_usage.total_tokens as u64;
            }
            AgentEvent::TurnFinished { usage, total_calls, duration_ms, history, total_usage, last_usage } => {
                let s = &mut self.sessions[target];
                s.agent.history = history;
                s.agent.last_usage = last_usage;
                s.agent.total_calls = total_calls;
                s.total_tokens = total_usage.total_tokens as u64;
                s.agent.total_usage = total_usage.clone();
                for step in s.react_steps.iter_mut().rev() {
                    if step.starts_with("●") {
                        *step = format!("✓ {}", step.trim_start_matches("● ").trim());
                    }
                }
                if let Some(msg) = s.messages.iter_mut().rev().find(|m| m.role == "assistant") {
                    msg.tokens = Some(usage);
                    msg.duration_ms = Some(duration_ms);
                    let content = msg.content.clone();
                    s.update_todos_from_text(&content);
                }
                return true;
            }
            AgentEvent::Error(e) => {
                let s = &mut self.sessions[target];
                s.messages.push(ChatMessage::plain("system", format!("Error: {}", e)));
                if is_active {
                    self.status_msg = "Turn failed".into();
                }
                return true;
            }
            AgentEvent::Cancelled => {
                let s = &mut self.sessions[target];
                s.messages.push(ChatMessage::plain("system", "[Interrupted by user (Ctrl+C) — history kept]".to_string()));
                if is_active {
                    self.status_msg = "Cancelled".into();
                }
                return true;
            }
        }
        false
    }

    /// Run /compress in the background so the UI keeps rendering.
    fn start_compress(&mut self) {
        let idx = self.active_session;
        let mut agent = self.sessions[idx].agent.clone();
        let tx = self.bg_tx.clone();
        self.bg_busy += 1;
        self.status_msg = "Compressing conversation history…".into();
        tokio::spawn(async move {
            let result = agent.compress_history().await.map_err(|e| e.to_string());
            let _ = tx.send(BgResult::Compressed { session: idx, history: agent.history, total_usage: agent.total_usage, result });
        });
    }

    /// Answer the oldest pending approval of the active session.
    fn answer_approval(&mut self, d: ApprovalDecision) {
        if let Some(t) = self.session_mut().approvals.pop_front() {
            let label = match &d {
                ApprovalDecision::Approve => "approved",
                ApprovalDecision::ApproveAlways => "approved (always for this session)",
                ApprovalDecision::Reject(_) => "rejected",
            };
            let summary = t.request.summary.clone();
            t.respond(d);
            self.session_mut().messages.push(ChatMessage::plain("system", format!("{} — {}", summary, label)));
            self.approval_scroll = 0;
            self.status_msg = format!("{} {}", summary, label);
        }
    }

    /// Workspace files for `@` autocomplete (cached 30 s, ignore-aware).
    fn workspace_files(&mut self) -> Vec<String> {
        let fresh = self.file_cache.as_ref().map(|(t, _)| t.elapsed() < Duration::from_secs(30)).unwrap_or(false);
        if !fresh {
            let root = std::env::current_dir().unwrap_or_default();
            let mut files: Vec<String> = cacode_core::tools::search::text_files(&root)
                .into_iter()
                .filter_map(|p| p.strip_prefix(&root).ok().map(|r| r.to_string_lossy().to_string()))
                .collect();
            files.sort();
            files.truncate(20_000);
            self.file_cache = Some((Instant::now(), files));
        }
        self.file_cache.as_ref().map(|(_, f)| f.clone()).unwrap_or_default()
    }

    fn push_history(&mut self, entry: &str) {
        if entry.trim().is_empty() || self.input_history.last().map(|l| l == entry).unwrap_or(false) {
            return;
        }
        self.input_history.push(entry.to_string());
        if self.input_history.len() > 2_000 {
            let excess = self.input_history.len() - 2_000;
            self.input_history.drain(0..excess);
        }
        if let Some(p) = history_path() {
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let body: String = self.input_history.iter().map(|l| format!("{}\n", l.replace('\n', "\\\\n"))).collect();
            let _ = fs::write(p, body);
        }
    }

    fn build_settings(config: &CaCodeConfig) -> SettingsState {
        let fields_meta = config.display_fields();
        let fields: Vec<SettingsField> = fields_meta
            .into_iter()
            .map(|(label, _env, key, _disp)| {
                let raw = match key {
                    "app_name" => Some(config.app_name.clone()),
                    "theme" => Some(config.theme.clone()),
                    "thinking_mode" => Some(config.thinking_mode.clone()),
                    "pro_license_key" => config.pro_license_key.clone(),
                    "openai_api_key" => config.openai_api_key.clone(),
                    "openai_base_url" => config.openai_base_url.clone(),
                    "anthropic_api_key" => config.anthropic_api_key.clone(),
                    "ollama_url" => config.ollama_url.clone(),
                    "ollama_model" => config.ollama_model.clone(),
                    "router9_url" => config.router9_url.clone(),
                    "router9_api_key" => config.router9_api_key.clone(),
                    "router9_model" => config.router9_model.clone(),
                    _ => None,
                };
                SettingsField {
                    label,
                    key,
                    value: raw.unwrap_or_default(),
                    is_secret: key.contains("api_key") || key.contains("license"),
                }
            })
            .collect();
        SettingsState { fields, selected: 0, editing: false }
    }

    fn provider_name(&self) -> &str {
        self.session().agent.provider.name()
    }

    fn model_name(&self) -> String {
        self.session().agent.provider.model().to_string()
    }

    fn thinking_mode(&self) -> &str {
        if self.config.thinking_mode.is_empty() {
            "Fast"
        } else {
            &self.config.thinking_mode
        }
    }

    fn save_settings(&mut self) {
        for f in &self.settings.fields {
            self.config.set_field(f.key, f.value.clone());
        }
        match self.config.save() {
            Ok(_) => {
                for s in &mut self.sessions {
                    configure_agent(&mut s.agent, &self.config);
                    s.agent.provider = ReActAgent::detect_provider_with(&s.agent.creds);
                }
                self.status_msg = "Saved ~/.cacode/config.toml (keys → encrypted vault) ✓".into();
            }
            Err(e) => self.status_msg = format!("Save failed: {}", e),
        }
    }

    fn backup_config(&mut self) {
        for f in &self.settings.fields {
            self.config.set_field(f.key, f.value.clone());
        }
        match dirs::home_dir() {
            Some(home) => {
                let backup_path = home.join(".cacode").join("backup.toml");
                // never write secrets in plaintext: they stay in the encrypted vault
                let mut public = self.config.clone();
                for k in cacode_core::config::SECRET_FIELDS {
                    public.set_field(k, String::new());
                }
                for cp in public.custom_providers.iter_mut() {
                    cp.api_key.clear();
                }
                match fs::write(&backup_path, toml::to_string_pretty(&public).unwrap_or_default()) {
                    Ok(_) => self.status_msg = format!("Backed up to {}", backup_path.display()),
                    Err(e) => self.status_msg = format!("Backup failed: {}", e),
                }
            }
            None => self.status_msg = "Home dir not found".into(),
        }
    }

    fn restore_config(&mut self) {
        match dirs::home_dir() {
            Some(home) => {
                let backup_path = home.join(".cacode").join("backup.toml");
                if backup_path.exists() {
                    match fs::read_to_string(&backup_path) {
                        Ok(content) => {
                            if let Ok(mut cfg) = toml::from_str::<CaCodeConfig>(&content) {
                                // backups carry no secrets: keep the current ones
                                if cfg.openai_api_key.is_none() { cfg.openai_api_key = self.config.openai_api_key.clone(); }
                                if cfg.anthropic_api_key.is_none() { cfg.anthropic_api_key = self.config.anthropic_api_key.clone(); }
                                if cfg.router9_api_key.is_none() { cfg.router9_api_key = self.config.router9_api_key.clone(); }
                                if cfg.antigravity_zen_api_key.is_none() { cfg.antigravity_zen_api_key = self.config.antigravity_zen_api_key.clone(); }
                                if cfg.pro_license_key.is_none() { cfg.pro_license_key = self.config.pro_license_key.clone(); }
                                for cp in cfg.custom_providers.iter_mut() {
                                    if cp.api_key.is_empty() {
                                        if let Some(cur) = self.config.custom_providers.iter().find(|c| c.id == cp.id) {
                                            cp.api_key = cur.api_key.clone();
                                        }
                                    }
                                }
                                self.config = cfg;
                                let _ = self.config.save();
                                for s in &mut self.sessions {
                                    configure_agent(&mut s.agent, &self.config);
                                }
                                self.settings = Self::build_settings(&self.config);
                                self.status_msg = format!("Restored from {}", backup_path.display());
                            } else {
                                self.status_msg = "Failed to parse backup.toml".into();
                            }
                        }
                        Err(e) => self.status_msg = format!("Restore read failed: {}", e),
                    }
                } else {
                    self.status_msg = "No backup.toml found in ~/.cacode/".into();
                }
            }
            None => self.status_msg = "Home dir not found".into(),
        }
    }

    fn new_session(&mut self) {
        if self.sessions.len() >= MAX_SESSIONS {
            self.status_msg = format!("Max {} sessions", MAX_SESSIONS);
            return;
        }
        self.session_mut().show_welcome = false;
        // default_session_name(idx) is 0-based: the 2nd tab is "Term 2"
        let n = self.sessions.len();
        let mut session = ChatSession::new(default_session_name(n), &self.config);
        session.show_welcome = false;
        self.sessions.push(session);
        self.active_session = self.sessions.len() - 1;
        self.status_msg = format!("New session: {}", self.session().name);
    }

    fn close_session(&mut self) {
        if self.sessions.len() <= 1 {
            self.status_msg = "Cannot close last session".into();
            return;
        }
        let name = self.sessions[self.active_session].name.clone();
        self.cancel_turn();
        self.sessions.remove(self.active_session);
        if self.active_session >= self.sessions.len() {
            self.active_session = self.sessions.len() - 1;
        }
        self.status_msg = format!("Closed: {}", name);
    }

    fn next_session(&mut self) {
        if !self.sessions.is_empty() {
            self.active_session = (self.active_session + 1) % self.sessions.len();
        }
    }

    fn prev_session(&mut self) {
        if !self.sessions.is_empty() {
            self.active_session = if self.active_session == 0 {
                self.sessions.len() - 1
            } else {
                self.active_session - 1
            };
        }
    }

    fn switch_page(&mut self, target: usize) {
        self.active_page = target % 5;
        if self.active_page == 1 || self.active_page == 2 {
            self.refresh_usage_data();
        }
        if self.active_page == 3 {
            self.refresh_skills();
        }
        if self.active_page == 0 {
            self.host_step = 0;
            self.host_last_tick = Instant::now();
        }
    }

    fn next_page(&mut self) {
        let next = (self.active_page + 1) % 5;
        self.switch_page(next);
    }

    fn prev_page(&mut self) {
        let prev = (self.active_page + 4) % 5;
        self.switch_page(prev);
    }

    fn db_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".cacode").join("usage.db"))
    }

    fn skills_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".cacode").join("skills"))
    }

    fn refresh_skills(&mut self) {
        let dir = match Self::skills_dir() {
            Some(d) => d,
            None => return,
        };
        if !dir.exists() {
            let _ = fs::create_dir_all(&dir);
        }
        let mut list = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let file_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("skill").to_string();
                        let mut name = file_name.clone();
                        let mut created = "2026-01-01".to_string();
                        let mut description = "".to_string();
                        let mut body = content.clone();

                        if let Some(stripped) = content.strip_prefix("---") {
                            if let Some(end_idx) = stripped.find("---") {
                                let fm = &stripped[..end_idx];
                                body = stripped[end_idx + 3..].trim().to_string();
                                for line in fm.lines() {
                                    let parts: Vec<&str> = line.splitn(2, ':').collect();
                                    if parts.len() == 2 {
                                        let k = parts[0].trim();
                                        let v = parts[1].trim().trim_matches('"').trim_matches('\'');
                                        match k {
                                            "name" => name = v.to_string(),
                                            "created" => created = v.to_string(),
                                            "description" => description = v.to_string(),
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }

                        list.push(SkillItem {
                            name,
                            created,
                            description,
                            content: body,
                        });
                    }
                }
            }
        }
        self.skills = list;
    }

    fn save_skill(&mut self, skill_name: &str, description: &str, content: &str) -> Result<()> {
        let dir = Self::skills_dir().ok_or_else(|| anyhow::anyhow!("No skills dir"))?;
        fs::create_dir_all(&dir)?;
        let safe_name = skill_name.to_lowercase().replace(' ', "-");
        let path = dir.join(format!("{}.md", safe_name));
        let created = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let file_content = format!(
            "---\nname: \"{}\"\ncreated: \"{}\"\ndescription: \"{}\"\n---\n\n{}",
            skill_name, created, description, content
        );
        fs::write(path, file_content)?;
        self.refresh_skills();
        Ok(())
    }

    fn delete_skill(&mut self, skill_name: &str) -> Result<bool> {
        let dir = Self::skills_dir().ok_or_else(|| anyhow::anyhow!("No skills dir"))?;
        let safe_name = skill_name.to_lowercase().replace(' ', "-");
        let path = dir.join(format!("{}.md", safe_name));
        if path.exists() {
            fs::remove_file(path)?;
            self.refresh_skills();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Real round-trip against the active provider using the same adapter
    /// the agent uses (no env-var guessing).
    async fn test_connection(&mut self) {
        let agent = &self.session().agent;
        let provider = agent.provider.clone();
        let creds = Credentials::from_config(&self.config);
        let msgs = vec![Message::user("ping — reply with: pong")];
        let req = cacode_core::llm::LlmRequest {
            model: provider.model(),
            system: "Reply with the single word pong.",
            messages: &msgs,
            tools: &[],
            tool_mode: cacode_core::llm::ToolMode::Native,
            max_tokens: 8,
            stream: false,
        };
        let t = cacode_core::llm::Timeouts { first_byte: Duration::from_secs(20), idle: Duration::from_secs(20) };
        let started = Instant::now();
        let client = cacode_core::llm::build_client();
        let res = cacode_core::llm::complete(&client, &provider, &creds, &req, t, &|_: &str| {}).await;
        self.status_msg = match res {
            Ok(r) => format!("✓ {}: connection OK in {} ms ({} tokens{})", provider.display_label(), started.elapsed().as_millis(), r.usage.total_tokens, if r.usage.estimated { ", est." } else { "" }),
            Err(e) => format!("✗ {} error: {}", provider.display_label(), truncate_chars(&e.to_string(), 160)),
        };
    }

    fn refresh_models_cache(&mut self) {
        // Preset high-value models available across free providers, 9router and BYOK
        self.models_cache = vec![
            "deepseek-v4-flash-free".into(),
            "mimo-v2.5-free".into(),
            "big-pickle".into(),
            "minimax-m2.5-free".into(),
            "ZA126_PRO".into(),
            "ZA126_OPUS".into(),
            "ZA126".into(),
            "ag/claude-sonnet-5-5-high".into(),
            "ag/claude-opus-5-5-high".into(),
            "ag/gemini-3.1-pro-high".into(),
            "ag/gemini-4-argon-high".into(),
            "oc/deepseek-v4-flash-free".into(),
            "openrouter/nvidia/nemotron-3-ultra-550b-a55b:free".into(),
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "claude-3-5-sonnet-20240620".into(),
            "llama3".into(),
        ];
    }

    fn refresh_usage_data(&mut self) {
        let db_path = match Self::db_path() {
            Some(p) if p.exists() => p,
            _ => {
                self.usage_rows.clear();
                self.report_rows.clear();
                return;
            }
        };
        let conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        if let Ok(rows) = conn.prepare(
            "SELECT timestamp, model, total_tokens, est_cost FROM token_usage ORDER BY id DESC LIMIT 200"
        ).and_then(|mut stmt| {
            let mapped = stmt.query_map([], |row| {
                Ok(UsageRow {
                    timestamp: row.get(0)?,
                    model: row.get(1)?,
                    total_tokens: row.get(2)?,
                    est_cost: row.get(3)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()
        }) {
            self.usage_rows = rows;
        }

        if let Ok(rows) = conn.prepare(
            "SELECT model, COUNT(*) as calls, SUM(prompt_tokens) as prompt, SUM(completion_tokens) as completion, SUM(total_tokens) as total, SUM(est_cost) as cost FROM token_usage GROUP BY model ORDER BY total DESC"
        ).and_then(|mut stmt| {
            let mapped = stmt.query_map([], |row| {
                Ok(ReportRow {
                    model: row.get(0)?,
                    calls: row.get(1)?,
                    prompt_tokens: row.get(2)?,
                    completion_tokens: row.get(3)?,
                    total_tokens: row.get(4)?,
                    est_cost: row.get(5)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()
        }) {
            self.report_rows = rows;
        }

        // Daily Heatmap & Traffic Analysis (last 14 days)
        if let Ok(rows) = conn.prepare(
            "SELECT substr(timestamp, 1, 10) as day, COUNT(*) as calls, SUM(total_tokens) as tokens, SUM(est_cost) as cost FROM token_usage GROUP BY day ORDER BY day DESC LIMIT 14"
        ).and_then(|mut stmt| {
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, f64>(3)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()
        }) {
            let max_tokens = rows.iter().map(|(_, _, tok, _)| *tok).max().unwrap_or(1).max(1);
            self.daily_rows = rows.into_iter().map(|(day, calls, tokens, cost)| {
                let ratio = (tokens as f64 / max_tokens as f64).min(1.0);
                let bar_len = (ratio * 16.0).round() as usize;
                let bar = format!("{} {:>3}%", "█".repeat(bar_len.max(1)), ((tokens * 100) / max_tokens));
                DailyTrafficRow {
                    day,
                    calls,
                    tokens,
                    cost,
                    bar,
                }
            }).collect();
        }
    }
}

/// Terminal options for CACode: alt screen + bracketed paste, no mouse
/// capture (keeps native text selection in the chat).
pub const TERMINAL_OPTIONS: catui::terminal::TerminalOptions =
    catui::terminal::TerminalOptions { mouse: false, bracketed_paste: true, alternate_screen: true };


#[derive(Default)]
pub struct TuiOptions {
    pub resume: Option<cacode_core::sessions::SessionRecord>,
    pub permission_mode: Option<PermissionMode>,
    pub model: Option<String>,
}

/// Rebuild the visible transcript from an agent history (resume).
fn messages_from_history(history: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for m in history {
        match m.role.as_str() {
            "user" => out.push(ChatMessage::plain("user", m.content.clone())),
            "assistant" if !m.content.trim().is_empty() => out.push(ChatMessage::plain("assistant", m.content.clone())),
            "tool" => {
                let name = m.name.clone().unwrap_or_default();
                let body = truncate_chars(&m.content, 600);
                if m.is_error {
                    out.push(ChatMessage::plain("tool_result_error", format!("[Error: {}]\n{}", name, body)));
                } else {
                    out.push(ChatMessage::plain("tool_result", format!("[Result: {}]\n{}", name, body)));
                }
            }
            _ => {}
        }
    }
    out
}

impl App {
    /// Load a stored session into the active tab.
    fn resume_into_active(&mut self, rec: cacode_core::sessions::SessionRecord) {
        let title = rec.title.clone();
        let id = rec.id.clone();
        let s = self.session_mut();
        s.agent.restore_session(rec);
        s.messages = messages_from_history(&s.agent.history);
        s.messages.push(ChatMessage::plain("system", format!("↺ Resumed session {} — {}", id, title)));
        s.total_tokens = s.agent.total_usage.total_tokens as u64;
        s.show_welcome = false;
    }
}

pub async fn run_tui(opts: TuiOptions) -> Result<()> {
    let config = CaCodeConfig::load().unwrap_or_default();

    // CATUI TerminalSession: raw mode + alt screen + paste, restored on drop
    // and on panic (a panic must never leave the terminal in raw mode).
    let mut session = catui::terminal::TerminalSession::enter(TERMINAL_OPTIONS)?;
    let terminal = &mut session.terminal;

    let mut app = App::new(config);
    if let Some(m) = opts.permission_mode {
        for s in &mut app.sessions {
            s.agent.permission_mode = m;
        }
    }
    if let Some(model) = &opts.model {
        cacode_core::commands::headless::set_model(&mut app.sessions[0].agent.provider, model);
    }
    if let Some(rec) = opts.resume {
        app.resume_into_active(rec);
    }
    let result = event_loop(terminal, &mut app).await;

    session.restore()?;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        app.drain_agent_events();
        terminal.draw(|f| ui(f, app))?;
        if app.should_quit {
            app.cancel_all_turns();
            return Ok(());
        }
        if app.is_thinking || app.sessions.iter().any(|s| s.is_busy()) {
            app.spinner_frame += 1;
        }
        if event::poll(std::time::Duration::from_millis(50))? {
            // catui::event::map drops key-release events (Windows).
            let Some(ev) = catui::event::map(event::read()?) else {
                continue;
            };
            if let catui::event::AppEvent::Paste(text) = &ev {
                if app.active_page == 0 {
                    handle_paste(app, text);
                }
                continue;
            }
            if let catui::event::AppEvent::Key(key) = ev {
                if handle_global_key(app, key) {
                    continue;
                }
                match app.active_page {
                    0 => handle_chat_key(app, key, terminal).await?,
                    1 => handle_monitor_key(app, key),
                    2 => handle_report_key(app, key),
                    3 => handle_skills_key(app, key),
                    4 => handle_settings_key(app, key).await,
                    _ => {}
                }
            }
        }
    }
}

fn handle_global_key(app: &mut App, key: KeyEvent) -> bool {
    // If exit confirmation modal is active, capture all keystrokes
    if app.show_exit_confirmation {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                app.should_quit = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.show_exit_confirmation = false;
            }
            _ => {}
        }
        return true;
    }

    // Approval dialog for the active session captures keys until answered.
    if app.active_page == 0 && !app.session().approvals.is_empty() {
        match (key.modifiers, key.code) {
            (_, KeyCode::Char('y')) | (_, KeyCode::Char('Y')) | (_, KeyCode::Enter) => app.answer_approval(ApprovalDecision::Approve),
            (_, KeyCode::Char('a')) | (_, KeyCode::Char('A')) => app.answer_approval(ApprovalDecision::ApproveAlways),
            (_, KeyCode::Char('n')) | (_, KeyCode::Char('N')) | (_, KeyCode::Esc) => app.answer_approval(ApprovalDecision::Reject(None)),
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => app.cancel_turn(),
            (_, KeyCode::Up) => app.approval_scroll = app.approval_scroll.saturating_sub(1),
            (_, KeyCode::Down) => app.approval_scroll = app.approval_scroll.saturating_add(1),
            (_, KeyCode::PageUp) => app.approval_scroll = app.approval_scroll.saturating_sub(10),
            (_, KeyCode::PageDown) => app.approval_scroll = app.approval_scroll.saturating_add(10),
            _ => {}
        }
        return true;
    }

    // Ctrl+D anywhere requests exit confirmation (unless editing setting value)
    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('d') && !app.settings.editing {
        app.show_exit_confirmation = true;
        return true;
    }

    // Esc in any secondary page returns straight to Chat
    if app.active_page != 0 && key.code == KeyCode::Esc && !app.settings.editing {
        app.switch_page(0);
        return true;
    }

    if app.active_page == 4 && app.settings.editing {
        return false;
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('1')) | (_, KeyCode::F(1)) => { app.switch_page(0); true }
        (KeyModifiers::CONTROL, KeyCode::Char('2')) | (_, KeyCode::F(2)) => { app.switch_page(1); true }
        (KeyModifiers::CONTROL, KeyCode::Char('3')) | (_, KeyCode::F(3)) => { app.switch_page(2); true }
        (KeyModifiers::CONTROL, KeyCode::Char('4')) | (_, KeyCode::F(4)) => { app.switch_page(3); true }
        (KeyModifiers::CONTROL, KeyCode::Char('5')) | (_, KeyCode::F(5)) => { app.switch_page(4); true }
        (KeyModifiers::CONTROL, KeyCode::Char('t')) | (_, KeyCode::F(10)) => {
            app.new_session();
            app.switch_page(0);
            true
        }
        (KeyModifiers::SHIFT, KeyCode::Tab) | (_, KeyCode::BackTab) if app.active_page != 0 => {
            app.prev_page();
            true
        }
        (_, KeyCode::F(6)) => {
            app.prev_page();
            true
        }
        (KeyModifiers::CONTROL, KeyCode::Tab) | (_, KeyCode::F(7)) => {
            app.next_page();
            true
        }
        _ if app.active_page != 0 => match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                app.next_page();
                true
            }
            KeyCode::Left | KeyCode::Char('h') => {
                app.prev_page();
                true
            }
            _ => false,
        },
        _ => false,
    }
}

async fn handle_chat_key(
    app: &mut App,
    key: KeyEvent,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<()> {
    // 1. If Modal Picker is active (Models or Thinking)
    if app.modal_picker != ModalPicker::Off {
        match key.code {
            KeyCode::Esc => {
                app.modal_picker = ModalPicker::Off;
                return Ok(());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if app.modal_idx > 0 { app.modal_idx -= 1; }
                return Ok(());
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max_len = match app.modal_picker {
                    ModalPicker::Models => app.models_cache.len(),
                    ModalPicker::Thinking => THINKING_MODES.len(),
                    ModalPicker::Off => 0,
                };
                if app.modal_idx + 1 < max_len { app.modal_idx += 1; }
                return Ok(());
            }
            KeyCode::Enter => {
                match app.modal_picker {
                    ModalPicker::Models => {
                        if let Some(m) = app.models_cache.get(app.modal_idx).cloned() {
                            match &mut app.session_mut().agent.provider {
                                Provider::OpenAI { model } => *model = m.clone(),
                                Provider::Anthropic { model } => *model = m.clone(),
                                Provider::Ollama { model } => *model = m.clone(),
                                Provider::Router9 { model } => *model = m.clone(),
                                Provider::AntigravityZen { model } => *model = m.clone(),
                                Provider::Custom { model, .. } => *model = m.clone(),
                            }
                            if matches!(app.session().agent.provider, Provider::AntigravityZen { .. }) {
                                app.config.antigravity_zen_model = Some(m.clone());
                            } else if matches!(app.session().agent.provider, Provider::Ollama { .. }) {
                                app.config.ollama_model = Some(m.clone());
                            } else {
                                app.config.router9_model = Some(m.clone());
                            }
                            let _ = app.config.save();
                            app.status_msg = format!("Model switched to {} ✓", m);
                        }
                    }
                    ModalPicker::Thinking => {
                        if let Some((mode_name, _)) = THINKING_MODES.get(app.modal_idx) {
                            app.config.thinking_mode = mode_name.to_string();
                            let _ = app.config.save();
                            app.status_msg = format!("Thinking mode set to {} ✓", mode_name);
                        }
                    }
                    ModalPicker::Off => {}
                }
                app.modal_picker = ModalPicker::Off;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }

    // 2a. Ctrl+R reverse history search
    if let Some(query) = app.history_search.clone() {
        match key.code {
            KeyCode::Esc => {
                app.history_search = None;
                app.session_mut().input.clear();
                app.session_mut().cursor_pos = 0;
                return Ok(());
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // next older match
                let cur = app.history_cursor.unwrap_or(app.input_history.len());
                if let Some(i) = app.input_history[..cur.min(app.input_history.len())].iter().rposition(|h| h.contains(&query)) {
                    app.history_cursor = Some(i);
                    let v = app.input_history[i].clone();
                    app.session_mut().cursor_pos = v.len();
                    app.session_mut().input = v;
                }
                return Ok(());
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let q = format!("{}{}", query, c);
                if let Some(i) = app.input_history.iter().rposition(|h| h.contains(&q)) {
                    app.history_cursor = Some(i);
                    let v = app.input_history[i].clone();
                    app.session_mut().cursor_pos = v.len();
                    app.session_mut().input = v;
                }
                app.history_search = Some(q);
                return Ok(());
            }
            KeyCode::Backspace => {
                let mut q = query.clone();
                q.pop();
                app.history_search = Some(q);
                return Ok(());
            }
            KeyCode::Enter => {} // fall through: submit the matched entry
            _ => {
                app.history_search = None;
            }
        }
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
        app.history_search = Some(String::new());
        app.history_cursor = None;
        app.status_msg = "(reverse-i-search) type to search history · Ctrl+R older · Enter run · Esc cancel".into();
        return Ok(());
    }

    // 2b. Prompt history: Alt/Ctrl + Up/Down
    if (key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::CONTROL)) && matches!(key.code, KeyCode::Up | KeyCode::Down) {
        if app.input_history.is_empty() {
            return Ok(());
        }
        let n = app.input_history.len();
        let next = match (key.code, app.history_cursor) {
            (KeyCode::Up, None) => Some(n - 1),
            (KeyCode::Up, Some(i)) => Some(i.saturating_sub(1)),
            (KeyCode::Down, Some(i)) if i + 1 < n => Some(i + 1),
            _ => None,
        };
        app.history_cursor = next;
        let v = next.map(|i| app.input_history[i].clone()).unwrap_or_default();
        app.session_mut().cursor_pos = v.len();
        app.session_mut().input = v;
        return Ok(());
    }

    // 2c. @file mention autocomplete (Tab completes, ↑↓ select)
    if let Some((start, needle)) = mention_at_cursor(&app.session().input, app.session().cursor_pos) {
        if matches!(key.code, KeyCode::Tab | KeyCode::Up | KeyCode::Down) {
            let files = app.workspace_files();
            let matches = match_files(&files, &needle, 8);
            if !matches.is_empty() {
                match key.code {
                    KeyCode::Tab => {
                        let choice = matches[app.autocomplete_idx % matches.len()].clone();
                        let pos = app.session().cursor_pos;
                        let mut input = app.session().input.clone();
                        input.replace_range(start..pos, &format!("@{} ", choice));
                        let new_pos = start + choice.len() + 2;
                        app.session_mut().input = input;
                        app.session_mut().cursor_pos = new_pos;
                        app.autocomplete_idx = 0;
                    }
                    KeyCode::Up => app.autocomplete_idx = (app.autocomplete_idx + matches.len() - 1) % matches.len(),
                    _ => app.autocomplete_idx = (app.autocomplete_idx + 1) % matches.len(),
                }
                return Ok(());
            }
        }
    }

    // 2. Autocomplete navigation when input starts with '/'
    let input_str = app.session().input.clone();
    let is_slash = input_str.starts_with('/');
    let filtered_cmds: Vec<(&str, &str)> = if is_slash {
        let needle = input_str.trim();
        SLASH_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(needle) || needle == "/" || cmd.contains(needle.trim_start_matches('/')))
            .copied()
            .collect()
    } else {
        Vec::new()
    };

    if is_slash && !filtered_cmds.is_empty() {
        match key.code {
            KeyCode::Tab => {
                let choice = filtered_cmds[app.autocomplete_idx % filtered_cmds.len()].0;
                app.session_mut().input = format!("{} ", choice);
                let len = app.session().input.len();
                app.session_mut().cursor_pos = len;
                app.autocomplete_idx = 0;
                return Ok(());
            }
            KeyCode::Up => {
                if app.autocomplete_idx > 0 {
                    app.autocomplete_idx -= 1;
                } else {
                    app.autocomplete_idx = filtered_cmds.len() - 1;
                }
                return Ok(());
            }
            KeyCode::Down => {
                app.autocomplete_idx = (app.autocomplete_idx + 1) % filtered_cmds.len();
                return Ok(());
            }
            _ => {}
        }
    } else {
        // Not in slash autocomplete: Tab and Shift+Tab cycle sessions.
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            if key.modifiers.contains(KeyModifiers::SHIFT) || key.code == KeyCode::BackTab {
                app.prev_session();
            } else {
                app.next_session();
            }
            return Ok(());
        }
    }

    // 3. Regular chat inputs & shortcuts
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('p')) => {
            if !app.session().input.starts_with('/') {
                app.session_mut().input = "/".to_string();
                let len = app.session().input.len();
                app.session_mut().cursor_pos = len;
                app.autocomplete_idx = 0;
            }
            return Ok(());
        }
        (KeyModifiers::SHIFT, KeyCode::Tab) | (_, KeyCode::BackTab) => {
            app.prev_session();
            return Ok(());
        }
        (_, KeyCode::Tab) => {
            app.next_session();
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if app.session().is_busy() {
                app.cancel_turn();
                app.status_msg = "Cancelling…".into();
            } else {
                app.show_exit_confirmation = true;
            }
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => {
            app.show_exit_confirmation = true;
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
            app.active_page = 4;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('t')) | (_, KeyCode::F(10)) => {
            app.new_session();
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
            app.close_session();
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Right) | (_, KeyCode::F(9)) => {
            app.next_session();
        }
        (KeyModifiers::CONTROL, KeyCode::Left) | (_, KeyCode::F(8)) => {
            app.prev_session();
        }
        (mods, KeyCode::Enter) if mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT) => {
            let pos = app.session().cursor_pos;
            let valid_pos = if app.session().input.is_char_boundary(pos) { pos } else { app.session().input.len() };
            app.session_mut().input.insert(valid_pos, '\n');
            app.session_mut().cursor_pos = valid_pos + 1;
            app.session_mut().show_welcome = false;
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            let pos = app.session().cursor_pos;
            let valid_pos = if app.session().input.is_char_boundary(pos) { pos } else { app.session().input.len() };
            app.session_mut().input.insert(valid_pos, '\n');
            app.session_mut().cursor_pos = valid_pos + 1;
            app.session_mut().show_welcome = false;
            return Ok(());
        }
        (_, KeyCode::Enter) => {
            app.history_search = None;
            let raw_input = app.session().input.trim().to_string();
            if raw_input.is_empty() { return Ok(()); }
            app.push_history(&raw_input);
            app.history_cursor = None;
            let input = app.session().expand_pastes(&raw_input);
            app.session_mut().pastes.clear();
            app.session_mut().input.clear();
            app.session_mut().cursor_pos = 0;
            app.session_mut().show_welcome = false;
            app.autocomplete_idx = 0;

            match app.session().wizard_state.clone() {
                WizardState::PrdStep1ProjectName => {
                    let project_name = input.clone();
                    app.session_mut().messages.push(ChatMessage::plain("user", project_name.clone()));
                    app.session_mut().wizard_state = WizardState::PrdStep2Stack { project_name };
                    let q2 = "🛠️ [PRD Wizard • 2/3] Bahasa pemrograman & arsitektur apa yang ingin digunakan?\n  1. (Recommended) Rust + Tauri v2 (High-performance, sovereign desktop, zero-crash)\n  2. Go + HTMX / Templ (Lightweight single binary)\n  3. TypeScript + SvelteKit / CAUI (Modern reactive UI)\n  4. Python + FastAPI (Rapid API prototype)\n  *(Ketik angka 1-4 atau ketik pilihan manual)*";
                    app.session_mut().messages.push(ChatMessage::plain("assistant", q2));
                    return Ok(());
                }
                WizardState::PrdStep2Stack { project_name } => {
                    let stack = match input.trim() {
                        "1" => "Rust + Tauri v2 (High-performance, sovereign desktop, zero-crash)".to_string(),
                        "2" => "Go + HTMX / Templ (Lightweight single binary)".to_string(),
                        "3" => "TypeScript + SvelteKit / CAUI (Modern reactive UI)".to_string(),
                        "4" => "Python + FastAPI (Rapid API prototype)".to_string(),
                        other => other.to_string(),
                    };
                    app.session_mut().messages.push(ChatMessage::plain("user", stack.clone()));
                    app.session_mut().wizard_state = WizardState::PrdStep3Structure { project_name, stack };
                    let q3 = "📐 [PRD Wizard • 3/3] Struktur dan modularitas seperti apa yang dibutuhkan?\n  1. (Recommended) Modular Workspace: Core Library + CLI + TUI + Local SQLite\n  2. Clean Architecture: Domain + UseCase + Infrastructure + Presentation\n  3. Microservices: API Gateway + Business Services + Background Worker\n  *(Ketik angka 1-3 atau ketik struktur manual)*";
                    app.session_mut().messages.push(ChatMessage::plain("assistant", q3));
                    return Ok(());
                }
                WizardState::PrdStep3Structure { project_name, stack } => {
                    let structure = match input.trim() {
                        "1" => "Modular Workspace: Core Library + CLI + TUI + Local SQLite".to_string(),
                        "2" => "Clean Architecture: Domain + UseCase + Infrastructure + Presentation".to_string(),
                        "3" => "Microservices: API Gateway + Business Services + Background Worker".to_string(),
                        other => other.to_string(),
                    };
                    app.session_mut().messages.push(ChatMessage::plain("user", structure.clone()));
                    app.session_mut().wizard_state = WizardState::None;
                    let synthesis_prompt = format!(
                        "🎯 [PRD WIZARD COMPLETE]\nAuthor a comprehensive, production-grade Product Requirements Document (PRD.md) in the current directory with the following specifications:\n\n- Project Name & Objective: {}\n- Target Stack & Architecture: {}\n- Modularity & Structure: {}\n\nStructure the PRD.md with:\n1. Executive Summary & Problem Statement\n2. Key Functional Requirements & Personas\n3. System Architecture & Tech Stack Justification\n4. Directory Structure & Modular Component Design\n5. API Endpoints / Local Data Schema\n6. Phased Implementation Milestones & QA Acceptance Criteria\n\nUse the `write_file` tool to save `PRD.md`.",
                        project_name, stack, structure
                    );
                    app.session_mut().messages.push(ChatMessage::plain("assistant", "Synthesizing requirements into PRD.md via ReAct agent...".to_string()));
                    app.session_mut().scroll_offset = 0;
                    app.start_turn(synthesis_prompt);
                    return Ok(());
                }
                WizardState::None => {}
            }

            if input == "@prd" || input == "/prd" {
                app.session_mut().wizard_state = WizardState::PrdStep1ProjectName;
                let q1 = "🎯 [PRD Wizard • 1/3] Apa nama dan tujuan utama proyek/aplikasi yang ingin Anda buat?";
                app.session_mut().messages.push(ChatMessage::plain("assistant", q1));
                return Ok(());
            } else if let Some(stripped) = input.strip_prefix("@prd ").or_else(|| input.strip_prefix("/prd ")) {
                let project_name = stripped.trim().to_string();
                app.session_mut().messages.push(ChatMessage::plain("user", project_name.clone()));
                app.session_mut().wizard_state = WizardState::PrdStep2Stack { project_name };
                let q2 = "🛠️ [PRD Wizard • 2/3] Bahasa pemrograman & arsitektur apa yang ingin digunakan?\n  1. (Recommended) Rust + Tauri v2 (High-performance, sovereign desktop, zero-crash)\n  2. Go + HTMX / Templ (Lightweight single binary)\n  3. TypeScript + SvelteKit / CAUI (Modern reactive UI)\n  4. Python + FastAPI (Rapid API prototype)\n  *(Ketik angka 1-4 atau ketik pilihan manual)*";
                app.session_mut().messages.push(ChatMessage::plain("assistant", q2));
                return Ok(());
            } else if input == "@dev" {
                app.session_mut().messages.push(ChatMessage::plain("assistant", "⚡ [Dev Protocol] Apa fitur atau perubahan yang ingin Anda implementasikan? (Ketik deskripsi tugas)"));
                return Ok(());
            } else if let Some(stripped) = input.strip_prefix("@dev ") {
                let task = stripped.trim();
                let dev_prompt = format!("⚡ [DEV PROTOCOL ACTIVATED]\nTask: {}\nExecute systematic implementation:\n1. Search and inspect existing files\n2. Author or patch implementation\n3. Run cargo test or validation to verify zero regressions.", task);
                app.session_mut().messages.push(ChatMessage::plain("user", input.clone()));
                app.session_mut().scroll_offset = 0;
                app.start_turn(dev_prompt);
                return Ok(());
            } else if input == "@audit" {
                app.session_mut().messages.push(ChatMessage::plain("assistant", "🛡️ [Audit Protocol] File, modul, atau aspek keamanan mana yang ingin diaudit?"));
                return Ok(());
            } else if let Some(stripped) = input.strip_prefix("@audit ") {
                let target = stripped.trim();
                let audit_prompt = format!("🛡️ [AUDIT PROTOCOL ACTIVATED]\nTarget: {}\nPerform whitebox audit on code safety, memory management, error handling, and architecture soundness. Report actionable findings.", target);
                app.session_mut().messages.push(ChatMessage::plain("user", input.clone()));
                app.session_mut().scroll_offset = 0;
                app.start_turn(audit_prompt);
                return Ok(());
            } else if input == "@fix" {
                app.session_mut().messages.push(ChatMessage::plain("assistant", "🐞 [Fix Protocol] Jelaskan bug, error log, atau perilaku tak terduga yang terjadi:"));
                return Ok(());
            } else if let Some(stripped) = input.strip_prefix("@fix ") {
                let bug = stripped.trim();
                let fix_prompt = format!("🐞 [FIX PROTOCOL ACTIVATED]\nIssue: {}\nApply systematic 4-phase bugfix:\n1. Understand root cause and find reproduction path\n2. Formulate minimal targeted patch\n3. Apply patch via patch_file or search_replace\n4. Verify with test/build.", bug);
                app.session_mut().messages.push(ChatMessage::plain("user", input.clone()));
                app.session_mut().scroll_offset = 0;
                app.start_turn(fix_prompt);
                return Ok(());
            }

            // Mid-turn steering: while a ReAct turn is in flight, a plain
            // message is captured as a `[Steering / Mid-turn]` instruction
            // that the background agent folds into its next reasoning step.
            if app.session().is_busy() && !input.starts_with('/') {
                app.session_mut()
                    .messages
                    .push(ChatMessage::plain("user", format!("[Steering / Mid-turn] {}", input)));
                app.session_mut().scroll_offset = 0;
                app.steer(input);
                app.status_msg = "Steering instruction queued ✓".into();
                return Ok(());
            }

            if input.starts_with('/') {
                let parts: Vec<&str> = input.splitn(2, ' ').collect();
                let cmd = parts[0];
                let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

                match cmd {
                    "/todo" => {
                        let sub_parts: Vec<&str> = arg.splitn(2, ' ').collect();
                        let sub = sub_parts.first().copied().unwrap_or("").trim();
                        let sub_arg = sub_parts.get(1).copied().unwrap_or("").trim();
                        match sub {
                            "add" => {
                                if sub_arg.is_empty() {
                                    app.session_mut().messages.push(ChatMessage::plain("system", "Usage: /todo add <task text>"));
                                } else {
                                    app.session_mut().todos.push(TodoItem {
                                        text: sub_arg.to_string(),
                                        done: false,
                                    });
                                    app.status_msg = format!("Added task: {}", sub_arg);
                                    app.session_mut().messages.push(ChatMessage::plain("system", format!("Task added: [ ] {}", sub_arg)));
                                }
                            }
                            "done" => {
                                if let Ok(idx) = sub_arg.parse::<usize>() {
                                    if idx >= 1 && idx <= app.session().todos.len() {
                                        app.session_mut().todos[idx - 1].done = true;
                                        let text = app.session().todos[idx - 1].text.clone();
                                        app.status_msg = format!("Task {} completed ✓", idx);
                                        app.session_mut().messages.push(ChatMessage::plain("system", format!("Task completed: [✓] {}", text)));
                                    } else {
                                        app.session_mut().messages.push(ChatMessage::plain("system", format!("Invalid task index {}. Use /todo list to check indices.", idx)));
                                    }
                                } else {
                                    app.session_mut().messages.push(ChatMessage::plain("system", "Usage: /todo done <1-based index>"));
                                }
                            }
                            "list" => {
                                if app.session().todos.is_empty() {
                                    app.session_mut().messages.push(ChatMessage::plain("system", "Task Checklist is empty. Add tasks with `/todo add <text>` or use `- [ ]` in messages."));
                                } else {
                                    let mut out = String::from("Task Checklist:\n");
                                    for (i, item) in app.session().todos.iter().enumerate() {
                                        let mark = if item.done { "[✓]" } else { "[ ]" };
                                        out.push_str(&format!("  {}. {} {}\n", i + 1, mark, item.text));
                                    }
                                    app.session_mut().messages.push(ChatMessage::plain("system", out.trim().to_string()));
                                }
                            }
                            "clear" => {
                                app.session_mut().todos.clear();
                                app.status_msg = "Task checklist cleared".into();
                                app.session_mut().messages.push(ChatMessage::plain("system", "Task checklist cleared."));
                            }
                            _ => {
                                app.session_mut().messages.push(ChatMessage::plain(
                                    "system",
                                    "Task Checklist Commands:\n  /todo add <text>   - Add pending task\n  /todo done <index> - Mark task as done (1-based)\n  /todo list         - Display checklist\n  /todo clear        - Clear all tasks",
                                ));
                            }
                        }
                        return Ok(());
                    }
                    "/settings" | "/setting" | "/set" | "/config" | "/cfg" => {
                        app.active_page = 4;
                        return Ok(());
                    }
                    "/monitor" | "/mon" | "/usage" => {
                        app.active_page = 1;
                        app.refresh_usage_data();
                        return Ok(());
                    }
                    "/report" | "/reports" | "/stat" | "/stats" => {
                        app.active_page = 2;
                        app.refresh_usage_data();
                        return Ok(());
                    }
                    "/skills" | "/snip" => {
                        app.active_page = 3;
                        app.refresh_skills();
                        return Ok(());
                    }
                    "/skill" => {
                        let skill_parts: Vec<&str> = arg.splitn(2, ' ').collect();
                        let sub = skill_parts.first().copied().unwrap_or("").trim();
                        let sub_arg = skill_parts.get(1).copied().unwrap_or("").trim();

                        match sub {
                            "add" => {
                                let add_parts: Vec<&str> = sub_arg.splitn(2, ' ').collect();
                                let skill_name = add_parts.first().copied().unwrap_or("").trim();
                                let inline_content = add_parts.get(1).copied().unwrap_or("").trim();

                                if skill_name.is_empty() {
                                    app.session_mut().messages.push(ChatMessage::plain("system", "Usage: /skill add <name> [content]"));
                                } else {
                                    let content_to_save = if !inline_content.is_empty() {
                                        inline_content.to_string()
                                    } else {
                                        app.session().messages.iter().rev().find(|m| m.role == "assistant").map(|m| m.content.clone()).unwrap_or_else(|| "No content".into())
                                    };
                                    match app.save_skill(skill_name, "Custom skill snippet", &content_to_save) {
                                        Ok(_) => app.status_msg = format!("Saved skill '{}' ✓", skill_name),
                                        Err(e) => app.status_msg = format!("Failed to save: {}", e),
                                    }
                                }
                            }
                            "del" | "delete" | "remove" => {
                                match app.delete_skill(sub_arg) {
                                    Ok(true) => app.status_msg = format!("Deleted skill '{}' ✓", sub_arg),
                                    Ok(false) => app.status_msg = format!("Skill '{}' not found", sub_arg),
                                    Err(e) => app.status_msg = format!("Delete error: {}", e),
                                }
                            }
                            _ => {
                                app.active_page = 3;
                                app.refresh_skills();
                            }
                        }
                        return Ok(());
                    }
                    "/models" => {
                        app.refresh_models_cache();
                        app.modal_picker = ModalPicker::Models;
                        app.modal_idx = 0;
                        return Ok(());
                    }
                    "/mode" => {
                        if arg.is_empty() {
                            app.modal_picker = ModalPicker::Thinking;
                            app.modal_idx = match app.config.thinking_mode.as_str() {
                                "Analytical" => 1,
                                "Deep" => 2,
                                _ => 0,
                            };
                            return Ok(());
                        } else {
                            let m = match arg.to_lowercase().as_str() {
                                "fast" | "1" => "Fast",
                                "analytical" | "2" => "Analytical",
                                "deep" | "ultra" | "3" => "Deep",
                                other => {
                                    app.session_mut().messages.push(ChatMessage::plain("system", format!("Unknown mode '{}'. Choose: fast, analytical, deep", other)));
                                    return Ok(());
                                }
                            };
                            app.config.thinking_mode = m.to_string();
                            let _ = app.config.save();
                            app.status_msg = format!("Thinking mode set to {} ✓", m);
                            return Ok(());
                        }
                    }
                    "/model" => {
                        if arg.is_empty() {
                            app.refresh_models_cache();
                            app.modal_picker = ModalPicker::Models;
                            app.modal_idx = 0;
                            return Ok(());
                        } else {
                            match &mut app.session_mut().agent.provider {
                                Provider::OpenAI { model } => *model = arg.to_string(),
                                Provider::Anthropic { model } => *model = arg.to_string(),
                                Provider::Ollama { model } => *model = arg.to_string(),
                                Provider::Router9 { model } => *model = arg.to_string(),
                                Provider::AntigravityZen { model } => *model = arg.to_string(),
                                Provider::Custom { model, .. } => *model = arg.to_string(),
                            }
                            if matches!(app.session().agent.provider, Provider::AntigravityZen { .. }) {
                                app.config.antigravity_zen_model = Some(arg.to_string());
                            } else if matches!(app.session().agent.provider, Provider::Ollama { .. }) {
                                app.config.ollama_model = Some(arg.to_string());
                            } else {
                                app.config.router9_model = Some(arg.to_string());
                            }
                            let _ = app.config.save();
                            app.status_msg = format!("Model changed to {}", arg);
                        }
                        return Ok(());
                    }
                    "/providers" => {
                        app.is_thinking = true;
                        app.status_msg = "Probing local & free providers...".into();
                        terminal.draw(|f| ui(f, app))?;

                        let providers = cacode_core::provider_registry::list_free_providers(
                            app.config.ollama_url.as_deref()
                        ).await;

                        app.is_thinking = false;
                        let cur_p = app.provider_name();
                        let cur_m = app.model_name();

                        let mut output = String::from("=== Sovereign Free & Local Provider Registry ===\n\n");
                        for (idx, p) in providers.iter().enumerate() {
                            let is_active = (p.id == "antigravity_zen" && cur_p == "Antigravity Zen")
                                || (p.id == "ollama" && cur_p == "Ollama");
                            let marker = if is_active { " [ACTIVE]" } else { "" };
                            output.push_str(&format!("[{}] {}{}\n", idx + 1, p.name, marker));
                            output.push_str(&format!("    Endpoint: {}\n", p.endpoint_or_url));
                            output.push_str(&format!("    Status:   {}\n", p.note));
                            output.push_str(&format!("    Default:  {}\n", p.default_model));
                            if !p.models.is_empty() {
                                output.push_str(&format!("    Models:   {}\n", p.models.join(", ")));
                            } else {
                                output.push_str("    Models:   (none discovered)\n");
                            }
                            if p.id == "antigravity_zen" {
                                output.push_str("    Switch:   /provider antigravity  or  /provider zen\n");
                            } else if p.id == "ollama" {
                                output.push_str("    Switch:   /provider ollama\n");
                            }
                            output.push('\n');
                        }
                        output.push_str(&format!("Current Active Provider: {} (model: {})\n", cur_p, cur_m));
                        output.push_str("Switch provider: `/provider <name>` (antigravity, zen, ollama, 9router, openai, anthropic)");

                        app.session_mut().messages.push(ChatMessage::plain("system", output));
                        app.status_msg = "Probed free providers ✓".into();
                        return Ok(());
                    }
                    "/provider" => {
                        let current_provider = app.provider_name().to_string();
                        let cur_model = app.model_name();
                        if arg.is_empty() {
                            let msg = format!(
                                "Active Provider: {} ({})\nUsage:\n  /provider <name>                          - Switch active provider\n  /provider list                            - List all endpoints & models\n  /provider add <name> <url> <key> <model>  - Add custom provider",
                                current_provider, cur_model
                            );
                            app.session_mut().messages.push(ChatMessage::plain("system", msg));
                        } else if arg == "list" {
                            let mut out = String::from("Configured Providers & Endpoints:\n");
                            out.push_str("  • Antigravity Zen: https://antigravity.ai/zen/v1\n");
                            out.push_str(&format!("  • Ollama: {}\n", app.config.effective("OLLAMA_URL", &app.config.ollama_url)));
                            out.push_str(&format!("  • 9Router: {}\n", app.config.effective("ROUTER9_URL", &app.config.router9_url)));
                            out.push_str(&format!("  • OpenAI: {}\n", app.config.effective("OPENAI_BASE_URL", &app.config.openai_base_url)));
                            out.push_str("  • Anthropic: https://api.anthropic.com/v1\n");
                            if !app.config.custom_providers.is_empty() {
                                out.push_str("\nCustom Providers:\n");
                                for cp in &app.config.custom_providers {
                                    out.push_str(&format!("  • {} (id: {}, model: {}, url: {})\n", cp.name, cp.id, cp.model, cp.base_url));
                                }
                            }
                            out.push_str(&format!("\nActive: {} (model: {})\n", app.provider_name(), app.model_name()));
                            out.push_str(&format!(
                                "Failover Chain: F1: {}, F2: {}, F3: {}\n",
                                app.config.fallback_1.as_deref().unwrap_or("none"),
                                app.config.fallback_2.as_deref().unwrap_or("none"),
                                app.config.fallback_3.as_deref().unwrap_or("none"),
                            ));
                            app.session_mut().messages.push(ChatMessage::plain("system", out));
                        } else if let Some(stripped) = arg.strip_prefix("add ") {
                            let sub_args: Vec<&str> = stripped.split_whitespace().collect();
                            if sub_args.len() < 4 {
                                app.session_mut().messages.push(ChatMessage::plain("system", "Usage: /provider add <name> <base_url> <key> <model>"));
                            } else {
                                let name = sub_args[0].to_string();
                                let base_url = sub_args[1].to_string();
                                let key = sub_args[2].to_string();
                                let model = sub_args[3].to_string();
                                let id = name.to_lowercase().replace(' ', "-");
                                app.config.custom_providers.retain(|cp| cp.id != id);
                                app.config.custom_providers.push(CustomProviderConfig {
                                    id: id.clone(),
                                    name: name.clone(),
                                    base_url: base_url.clone(),
                                    api_key: key,
                                    model: model.clone(),
                                });
                                let _ = app.config.save();
                                app.status_msg = format!("Added custom provider '{}' ✓", name);
                                app.session_mut().messages.push(ChatMessage::plain(
                                    "system",
                                    format!("Added custom provider '{}' (endpoint: {}, model: {}).\nSwitch to it with: `/provider {}`", name, base_url, model, id),
                                ));
                            }
                        } else if let Some(new_p) = resolve_provider_string(arg, &app.config) {
                            let name = new_p.name().to_string();
                            let m = new_p.model().to_string();
                            app.session_mut().agent.provider = new_p;
                            app.status_msg = format!("Provider switched to {} ({}) ✓", name, m);
                            app.session_mut().messages.push(ChatMessage::plain("system", format!("Active provider switched to {} (model: {})", name, m)));
                        } else {
                            app.session_mut().messages.push(ChatMessage::plain(
                                "system",
                                format!("Unknown provider: '{}'. Type `/provider list` to see available providers or `/provider add` to create one.", arg),
                            ));
                        }
                        return Ok(());
                    }
                    "/fallback" => {
                        let parts: Vec<&str> = arg.split_whitespace().collect();
                        if parts.is_empty() {
                            let f1 = app.config.fallback_1.as_deref().unwrap_or("none").to_string();
                            let f2 = app.config.fallback_2.as_deref().unwrap_or("none").to_string();
                            let f3 = app.config.fallback_3.as_deref().unwrap_or("none").to_string();
                            let msg = format!(
                                "Current Fallback Routing Tiers:\n  Fallback Tier 1: {}\n  Fallback Tier 2: {}\n  Fallback Tier 3: {}\n\nUsage: /fallback <1|2|3> <provider>\nExample: /fallback 1 9router",
                                f1, f2, f3
                            );
                            app.session_mut().messages.push(ChatMessage::plain("system", msg));
                        } else if parts.len() >= 2 {
                            let tier = parts[0];
                            let target_prov = parts[1];
                            if resolve_provider_string(target_prov, &app.config).is_some() {
                                match tier {
                                    "1" => app.config.fallback_1 = Some(target_prov.to_string()),
                                    "2" => app.config.fallback_2 = Some(target_prov.to_string()),
                                    "3" => app.config.fallback_3 = Some(target_prov.to_string()),
                                    _ => {
                                        app.session_mut().messages.push(ChatMessage::plain("system", "Invalid tier. Choose 1, 2, or 3. E.g.: /fallback 1 9router"));
                                        return Ok(());
                                    }
                                }
                                let _ = app.config.save();
                                let new_chain = build_fallback_chain(&app.config);
                                for s in &mut app.sessions {
                                    s.agent.fallback_chain = new_chain.clone();
                                }
                                app.status_msg = format!("Fallback Tier {} set to {} ✓", tier, target_prov);
                                app.session_mut().messages.push(ChatMessage::plain(
                                    "system",
                                    format!("Fallback Tier {} configured to: {}", tier, target_prov),
                                ));
                            } else {
                                app.session_mut().messages.push(ChatMessage::plain(
                                    "system",
                                    format!("Unknown provider '{}'. Use /provider list to inspect valid providers.", target_prov),
                                ));
                            }
                        } else {
                            app.session_mut().messages.push(ChatMessage::plain("system", "Usage: /fallback <1|2|3> <provider>"));
                        }
                        return Ok(());
                    }
                    "/theme" => {
                        if arg.is_empty() {
                            let current_theme = app.theme.name;
                            app.session_mut().messages.push(ChatMessage::plain(
                                "system",
                                format!(
                                    "Available Themes (CATUI):\n{}\n\nActive Theme: {}\nUsage: /theme <name>   (legacy: classic, tokyo, onedark, contrast, light)",
                                    ThemePreset::ALL
                                        .iter()
                                        .map(|p| format!("  • {:<14} {}", p.id(), p.label()))
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                    current_theme
                                ),
                            ));
                        } else {
                            let new_theme = cacode_theme(arg);
                            app.theme = new_theme;
                            app.config.theme = arg.to_lowercase();
                            let _ = app.config.save();
                            let theme_name = app.theme.name;
                            let theme_preset = app.theme.preset;
                            app.status_msg = format!("Theme switched to {} ✓", theme_name);
                            app.session_mut().messages.push(ChatMessage::plain(
                                "system",
                                format!("Theme palette updated to {} (preset: {:?})", theme_name, theme_preset),
                            ));
                        }
                        return Ok(());
                    }
                    "/pro" => {
                        if arg.is_empty() {
                            let status = if app.is_pro() {
                                "⭐ ACTIVE [PRO EDITION UNLOCKED]"
                            } else {
                                "FREE EDITION (Standard)"
                            };
                            let card = format!(
                                "╔══════════════════════════════════════════════════════════════════════════════╗\n\
                                 ║                          CACODE PRO DIFFERENTIATION                          ║\n\
                                 ╠════════════════════════════════════════╦═════════════════════════════════════╣\n\
                                 ║ FREE EDITION                           ║ PRO EDITION ($10/mo or Sovereign)   ║\n\
                                 ╠════════════════════════════════════════╬═════════════════════════════════════╣\n\
                                 ║ • Single AI provider                   ║ • 3-Tier Auto-Fallback & Zero-Down  ║\n\
                                 ║ • Standard chat REPL                   ║ • Interactive Skill Wizards (@prd)  ║\n\
                                 ║ • Classic dark theme only              ║ • 6 Premium Themes + Light Mode     ║\n\
                                 ║ • Basic context window                 ║ • Ultra Context Compressor          ║\n\
                                 ║ • Single session                       ║ • Multi-session Workspace (1-9)     ║\n\
                                 ║ • Sequential execution                 ║ • Parallel Task Spawning            ║\n\
                                 ╚════════════════════════════════════════╩═════════════════════════════════════╝\n\n\
                                 Current Status: {}\n\
                                 To activate Pro, run: `/pro <license-key>`",
                                status
                            );
                            app.session_mut().messages.push(ChatMessage::plain("system", card));
                        } else {
                            app.config.pro_license_key = Some(arg.to_string());
                            let _ = app.config.save();
                            app.status_msg = "⭐ Pro license activated! [PRO] ✓".into();
                            app.session_mut().messages.push(ChatMessage::plain(
                                "system",
                                format!("⭐ CACode PRO successfully activated with key '{}'!\nAll premium features unlocked: 3-Tier Auto-Fallback, Skill Wizards, Premium Themes, and Multi-Session Workspace.", arg),
                            ));
                        }
                        return Ok(());
                    }
                    "/help" => {
                        app.session_mut().messages.push(ChatMessage::plain("system", "Available Commands:\n  /providers         - Probe & display local & free providers (Ollama, Zen)\n  /provider <name>   - Switch or configure provider (/provider add|list|<name>)\n  /fallback <1|2|3>  - Configure 3-tier auto-failover routing\n  /theme <name>      - Switch theme preset (classic, tokyo, nord, onedark, contrast, light)\n  /pro [key]         - Pro license activation & feature matrix\n  /models            - Pick model interactively\n  /mode              - Set thinking level (Fast, Analytical, Deep)\n  /settings          - Open BYOK config\n  /monitor           - View token telemetry log\n  /report            - View cost report\n  /skills            - Skills library\n  /todo <args>       - Task checklist (/todo add|done|list|clear)\n  /dev, @dev         - Development implementation protocol\n  /prd, @prd         - Interactive PRD authoring wizard\n  /audit, @audit     - Security & architecture whitebox audit\n  /fix, @fix         - Systematic 4-phase bugfix\n  /status            - Current system status\n  /sessions          - List chat sessions\n  /clear             - Clear chat\n  /export            - Export transcript to Markdown\n  /compress          - Summarise older context (runs in background)\n  /undo              - Revert the agent's last file edit\n  /permissions <m>   - ask | auto-edit | yolo | read-only\n  /jobs              - Background shell jobs\n  /version           - Show version\n  /exit              - Quit\nShortcuts: Tab (autocomplete), @path (file mention), Ctrl+R (history search), Alt/Ctrl+↑↓ (history), Shift+Enter (newline), Ctrl+T (new session), Ctrl+W (close), Ctrl+C (cancel turn), Esc (back to chat)\nApproval dialog: y approve · a always (session) · n reject · ↑↓ scroll diff"));
                        return Ok(());
                    }
                    "/clear" => {
                        app.session_mut().messages.clear();
                        app.session_mut().agent.history.clear();
                        app.session_mut().agent.session_id = cacode_core::sessions::new_id();
                        app.session_mut().total_tokens = 0;
                        app.session_mut().show_welcome = true;
                        app.session_mut().react_steps.clear();
                        app.session_mut().todos.clear();
                        app.status_msg = "Cleared".into();
                        return Ok(());
                    }
                    "/compress" => {
                        if app.session().is_busy() {
                            app.status_msg = "Wait for the current turn to finish (or Ctrl+C) before /compress".into();
                            return Ok(());
                        }
                        app.start_compress();
                        return Ok(());
                    }
                    "/undo" => {
                        let res = app.session_mut().agent.ensure_tool_ctx().and_then(|c| c.undo.undo_last());
                        let msg = match res {
                            Ok(m) => format!("↶ {}", m),
                            Err(e) => format!("Undo: {}", e),
                        };
                        app.session_mut().messages.push(ChatMessage::plain("system", msg));
                        return Ok(());
                    }
                    "/permissions" | "/perm" => {
                        if let Some(m) = PermissionMode::parse(arg) {
                            app.config.permission_mode = m.label().to_string();
                            let _ = app.config.save();
                            for s in &mut app.sessions {
                                s.agent.permission_mode = m;
                            }
                            app.status_msg = format!("Permission mode: {}", m.label());
                        }
                        let cur = app.session().agent.permission_mode;
                        app.session_mut().messages.push(ChatMessage::plain(
                            "system",
                            format!(
                                "Permission mode: {}\n  ask        — confirm every edit (diff shown) and non-allowlisted command\n  auto-edit  — apply edits automatically, confirm commands\n  yolo       — never ask (trusted/CI only)\n  read-only  — deny edits and non-allowlisted commands\nUsage: /permissions <mode>",
                                cur.label()
                            ),
                        ));
                        return Ok(());
                    }
                    "/resume" => {
                        let store = cacode_core::sessions::SessionStore::open_default();
                        if app.session().is_busy() {
                            app.status_msg = "Finish or cancel the current turn first".into();
                            return Ok(());
                        }
                        if arg.is_empty() {
                            let ws = std::env::current_dir().ok().and_then(|p| p.canonicalize().ok());
                            let list = store.list(ws.as_deref());
                            let body = if list.is_empty() {
                                "No saved sessions for this directory.".to_string()
                            } else {
                                let mut b = String::from("Saved sessions (newest first) — /resume <id>:\n");
                                for s in list.iter().take(15) {
                                    b.push_str(&format!("  {}  {:>3} msgs  {}\n", s.id, s.messages, s.title));
                                }
                                b
                            };
                            app.session_mut().messages.push(ChatMessage::plain("system", body));
                        } else {
                            match store.load(arg) {
                                Ok(rec) => app.resume_into_active(rec),
                                Err(e) => app.session_mut().messages.push(ChatMessage::plain("system", format!("Resume failed: {}", e))),
                            }
                        }
                        return Ok(());
                    }
                    "/plan" => {
                        for s in &mut app.sessions {
                            s.agent.permission_mode = PermissionMode::Plan;
                        }
                        app.status_msg = "Plan mode: the agent investigates read-only and asks you to approve a plan".into();
                        app.session_mut().messages.push(ChatMessage::plain("system", "🗺 Plan mode ON — edits are blocked until you approve the agent's plan (y = ask mode, a = auto-edit). /permissions ask to leave."));
                        return Ok(());
                    }
                    "/trust" => {
                        let cwd = std::env::current_dir().unwrap_or_default();
                        let msg = match cacode_core::trust::trust(&cwd) {
                            Ok(()) => {
                                // rebuild tool contexts so hooks / MCP load on the next turn
                                for s in &mut app.sessions {
                                    s.agent.tool_ctx = None;
                                    s.agent.extensions_announced = false;
                                }
                                format!("Trusted {} — project hooks (.cacode/hooks.toml) and MCP servers (.mcp.json) load from the next turn.", cwd.display())
                            }
                            Err(e) => format!("Trust failed: {}", e),
                        };
                        app.session_mut().messages.push(ChatMessage::plain("system", msg));
                        return Ok(());
                    }
                    "/mcp" => {
                        let body = match app.session().agent.tool_ctx.as_ref() {
                            Some(c) => {
                                let mut b = format!("Workspace trusted: {}\nHooks: {} pre_tool, {} post_tool\n", c.trusted, c.hooks.pre.len(), c.hooks.post.len());
                                match &c.mcp {
                                    Some(m) => {
                                        let specs = m.specs();
                                        b.push_str(&format!("MCP tools ({}):\n", specs.len()));
                                        for t in specs.iter().take(60) {
                                            b.push_str(&format!("  {}{}\n", t.name, if t.read_only { " (read-only)" } else { "" }));
                                        }
                                    }
                                    None => b.push_str("No MCP servers configured (~/.cacode/mcp.json, or .mcp.json in a trusted workspace).\n"),
                                }
                                b
                            }
                            None => "MCP/hooks load when the first turn starts.".into(),
                        };
                        app.session_mut().messages.push(ChatMessage::plain("system", body));
                        return Ok(());
                    }
                    "/jobs" => {
                        let list = app.session().agent.tool_ctx.as_ref().map(|c| c.jobs.list()).unwrap_or_default();
                        let body = if list.is_empty() {
                            "No background jobs.".to_string()
                        } else {
                            list.iter().map(|(id, cmd, done)| format!("  {} [{}] {}", id, if *done { "done" } else { "running" }, cmd)).collect::<Vec<_>>().join("\n")
                        };
                        app.session_mut().messages.push(ChatMessage::plain("system", body));
                        return Ok(());
                    }
                    "/context" => {
                        let agent = &app.session().agent;
                        let last_u = agent.last_usage.clone();
                        let tot_u = agent.total_usage.clone();
                        let limit = agent.context_limit();
                        // occupancy = what the LAST request actually sent, not the lifetime total
                        let occupied = last_u.prompt_tokens + last_u.completion_tokens;
                        let pct = if limit > 0 { (occupied as f64 / limit as f64) * 100.0 } else { 0.0 };
                        let info = format!(
                            "Context Telemetry:\n  Active Model: {}\n  Context window: {} tokens\n  Current occupancy (last request): {} tokens (~{:.1}%){}\n  Messages in history: {}\n  Lifetime usage this session: {} tokens ({} in / {} out){}\n  Prompt cache this session: {} tokens read, {} written\n  Auto-compact triggers at {:.0}% of the window",
                            app.model_name(),
                            limit,
                            occupied,
                            pct,
                            if last_u.estimated { " (estimated)" } else { "" },
                            agent.history.len(),
                            tot_u.total_tokens,
                            tot_u.prompt_tokens,
                            tot_u.completion_tokens,
                            if tot_u.estimated { " — includes estimates" } else { "" },
                            tot_u.cache_read_tokens,
                            tot_u.cache_write_tokens,
                            cacode_core::commands::chat::AUTO_COMPACT_RATIO * 100.0
                        );
                        app.session_mut().messages.push(ChatMessage::plain("system", info));
                        return Ok(());
                    }
                    "/refact" | "/update" | "/bughunt" | "/test" | "/qa" => {
                        let skill_name = cmd.trim_start_matches('/');
                        let skill_file = format!("{}.md", skill_name);
                        let dir = App::skills_dir();
                        let mut skill_body = String::new();
                        if let Some(d) = dir {
                            if let Ok(content) = fs::read_to_string(d.join(skill_file)) {
                                if let Some(stripped) = content.strip_prefix("---") {
                                    if let Some(end_idx) = stripped.find("---") {
                                        skill_body = stripped[end_idx + 3..].trim().to_string();
                                    }
                                }
                            }
                        }
                        if skill_body.is_empty() {
                            skill_body = format!("Workflow: {}", skill_name);
                        }
                        if arg.is_empty() {
                            // Load skill prompt into user input for editing
                            app.session_mut().input = format!("{} ", skill_body);
                            let len = app.session().input.len();
                            app.session_mut().cursor_pos = len;
                            app.status_msg = format!("Loaded skill template /{} into input", skill_name);
                            return Ok(());
                        } else {
                            // Prepend skill instructions to the prompt and dispatch
                            // through the non-blocking agent runtime.
                            let full_prompt = format!("[Skill Directive: {}]\n\n{}\n\nTask:\n{}", skill_name.to_uppercase(), skill_body, arg);
                            app.session_mut()
                                .messages
                                .push(ChatMessage::plain("user", full_prompt.clone()));
                            app.session_mut().scroll_offset = 0;
                            app.start_turn(full_prompt);
                            return Ok(());
                        }
                    }
                    "/exit" => { app.show_exit_confirmation = true; return Ok(()); }
                    "/rename" => {
                        if arg.is_empty() {
                            app.session_mut().messages.push(ChatMessage::plain("system", "Usage: /rename <name>"));
                        } else {
                            app.session_mut().name = arg.to_string();
                            app.status_msg = format!("Renamed to: {}", arg);
                        }
                        return Ok(());
                    }
                    "/status" => {
                        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into());
                        let cfg_path = CaCodeConfig::config_path().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into());
                        let info = format!(
                            "Status:\n  Provider: {}\n  Model: {}\n  Thinking Mode: {}\n  Session Tokens: {}\n  Active Sessions: {}\n  CWD: {}\n  Config: {}",
                            app.provider_name(), app.model_name(), app.thinking_mode(), app.session().total_tokens, app.sessions.len(), cwd, cfg_path
                        );
                        app.session_mut().messages.push(ChatMessage::plain("system", info));
                        return Ok(());
                    }
                    "/sessions" => {
                        let mut list = String::from("Sessions:\n");
                        for (i, s) in app.sessions.iter().enumerate() {
                            let active = if i == app.active_session { " (active)" } else { "" };
                            list.push_str(&format!("  {}. {}{} [{} messages]\n", i + 1, s.name, active, s.messages.len()));
                        }
                        app.session_mut().messages.push(ChatMessage::plain("system", list.trim().to_string()));
                        return Ok(());
                    }
                    "/export" => {
                        if let Some(home) = dirs::home_dir() {
                            let export_dir = home.join(".cacode").join("exports");
                            let _ = fs::create_dir_all(&export_dir);
                            let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
                            let file_path = export_dir.join(format!("session-{}.md", ts));
                            let mut md = format!("# CACode Session Export: {}\nTimestamp: {}\nProvider: {} ({})\n\n", app.session().name, ts, app.provider_name(), app.model_name());
                            for m in &app.session().messages {
                                md.push_str(&format!("### {}\n\n{}\n\n", m.role, m.content));
                            }
                            match fs::write(&file_path, md) {
                                Ok(_) => app.status_msg = format!("Exported to {}", file_path.display()),
                                Err(e) => app.status_msg = format!("Export failed: {}", e),
                            }
                        }
                        return Ok(());
                    }
                    "/tokens" => {
                        let info = format!("Token Usage for session '{}': total ~{} tokens", app.session().name, app.session().total_tokens);
                        app.session_mut().messages.push(ChatMessage::plain("system", info));
                        return Ok(());
                    }
                    "/reset" => {
                        app.session_mut().agent.history.clear();
                        app.status_msg = "Agent context reset (view retained)".into();
                        return Ok(());
                    }
                    "/cd" => {
                        if arg.is_empty() {
                            let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                            app.session_mut().messages.push(ChatMessage::plain("system", format!("CWD: {}", cwd)));
                        } else {
                            match std::env::set_current_dir(arg) {
                                Ok(_) => {
                                    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                                    app.status_msg = format!("Changed CWD to {}", cwd);
                                }
                                Err(e) => {
                                    app.session_mut().messages.push(ChatMessage::plain("system", format!("cd error: {}", e)));
                                }
                            }
                        }
                        return Ok(());
                    }
                    "/pwd" => {
                        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into());
                        app.session_mut().messages.push(ChatMessage::plain("system", cwd));
                        return Ok(());
                    }
                    "/version" => {
                        app.session_mut().messages.push(ChatMessage::plain("system", format!("CACode v{} (Sovereign Rust Agent)", CA_VERSION)));
                        return Ok(());
                    }
                    s => {
                        app.session_mut().messages.push(ChatMessage::plain("system", format!("Unknown command: {}. Type /help or / for suggestions.", s),
                       ));
                        return Ok(());
                    }
                }
            }

            // Chat dispatch — spawn a non-blocking background ReAct turn.
            app.session_mut()
                .messages
                .push(ChatMessage::plain("user", input.clone()));
            app.session_mut().scroll_offset = 0;
            app.session_mut().update_todos_from_text(&input);

            // Append thinking mode hint if active
            let mut prompt_to_send = match app.config.thinking_mode.as_str() {
                "Analytical" => format!("[Mode: Analytical — break down problem step-by-step before answering]\n\n{}", input),
                "Deep" => format!("[Mode: Deep Reasoning — explore edge-cases, formal proof, and comprehensive verification]\n\n{}", input),
                _ => input.clone(),
            };
            // @path mentions → tell the agent which files the user means
            let mentioned = mentioned_files(&input);
            if !mentioned.is_empty() {
                prompt_to_send.push_str(&format!("\n\n(User referenced these workspace files: {}. Read them with read_file before answering.)", mentioned.join(", ")));
            }

            app.start_turn(prompt_to_send);
        }
        (_, KeyCode::Backspace) => {
            let pos = app.session().cursor_pos;
            if pos > 0 {
                let prev = app.session().input[..pos].char_indices().last().map(|(idx, _)| idx).unwrap_or(0);
                app.session_mut().input.remove(prev);
                app.session_mut().cursor_pos = prev;
            }
        }
        (_, KeyCode::Delete) => {
            let pos = app.session().cursor_pos;
            let len = app.session().input.len();
            if pos < len && app.session().input.is_char_boundary(pos) {
                app.session_mut().input.remove(pos);
            }
        }
        (_, KeyCode::Left)  => {
            let pos = app.session().cursor_pos;
            if pos > 0 {
                let prev = app.session().input[..pos].char_indices().last().map(|(idx, _)| idx).unwrap_or(0);
                app.session_mut().cursor_pos = prev;
            }
        }
        (_, KeyCode::Right) => {
            let pos = app.session().cursor_pos;
            let len = app.session().input.len();
            if pos < len {
                let next = app.session().input[pos..].chars().next().map(|c| pos + c.len_utf8()).unwrap_or(len);
                app.session_mut().cursor_pos = next;
            }
        }
        (_, KeyCode::Home)  => {
            let pos = app.session().cursor_pos;
            if pos > 0 {
                if let Some(nl) = app.session().input[..pos].rfind('\n') {
                    if pos == nl + 1 {
                        app.session_mut().cursor_pos = 0;
                    } else {
                        app.session_mut().cursor_pos = nl + 1;
                    }
                } else {
                    app.session_mut().cursor_pos = 0;
                }
            }
        }
        (_, KeyCode::End)   => {
            let pos = app.session().cursor_pos;
            let len = app.session().input.len();
            if pos < len {
                if let Some(nl) = app.session().input[pos..].find('\n') {
                    let line_end = pos + nl;
                    if pos == line_end {
                        app.session_mut().cursor_pos = len;
                    } else {
                        app.session_mut().cursor_pos = line_end;
                    }
                } else {
                    app.session_mut().cursor_pos = len;
                }
            }
        }
        (_, KeyCode::Up)    => {
            if app.session().input.contains('\n') {
                let (row, col) = get_cursor_coords(&app.session().input, app.session().cursor_pos);
                if row > 0 {
                    let lines: Vec<&str> = app.session().input.split('\n').collect();
                    let target_row = row - 1;
                    let target_line = lines[target_row];
                    let target_col = col.min(target_line.chars().count());
                    let byte_offset: usize = target_line.chars().take(target_col).map(|c| c.len_utf8()).sum();
                    let mut prev_lines_len = 0;
                    for l in lines.iter().take(target_row) {
                        prev_lines_len += l.len() + 1;
                    }
                    app.session_mut().cursor_pos = prev_lines_len + byte_offset;
                } else {
                    let v = app.session().scroll_offset.saturating_add(1);
                    app.session_mut().scroll_offset = v;
                }
            } else {
                let v = app.session().scroll_offset.saturating_add(1);
                app.session_mut().scroll_offset = v;
            }
        }
        (_, KeyCode::Down)  => {
            if app.session().input.contains('\n') {
                let (row, col) = get_cursor_coords(&app.session().input, app.session().cursor_pos);
                let lines: Vec<&str> = app.session().input.split('\n').collect();
                if row + 1 < lines.len() {
                    let target_row = row + 1;
                    let target_line = lines[target_row];
                    let target_col = col.min(target_line.chars().count());
                    let byte_offset: usize = target_line.chars().take(target_col).map(|c| c.len_utf8()).sum();
                    let mut prev_lines_len = 0;
                    for l in lines.iter().take(target_row) {
                        prev_lines_len += l.len() + 1;
                    }
                    app.session_mut().cursor_pos = prev_lines_len + byte_offset;
                } else {
                    let v = app.session().scroll_offset.saturating_sub(1);
                    app.session_mut().scroll_offset = v;
                }
            } else {
                let v = app.session().scroll_offset.saturating_sub(1);
                app.session_mut().scroll_offset = v;
            }
        }
        (_, KeyCode::Char(c)) => {
            let pos = app.session().cursor_pos;
            let valid_pos = if app.session().input.is_char_boundary(pos) { pos } else { app.session().input.len() };
            app.session_mut().input.insert(valid_pos, c);
            app.session_mut().cursor_pos = valid_pos + c.len_utf8();
            app.session_mut().show_welcome = false;
        }
        _ => {}
    }
    Ok(())
}

fn handle_monitor_key(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => { app.active_page = 0; }
        (_, KeyCode::Char('r')) => { app.refresh_usage_data(); app.status_msg = "Refreshed".into(); }
        (_, KeyCode::Up)   => { app.monitor_scroll = app.monitor_scroll.saturating_add(1); }
        (_, KeyCode::Down) => { app.monitor_scroll = app.monitor_scroll.saturating_sub(1); }
        _ => {}
    }
}

fn handle_report_key(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => { app.active_page = 0; }
        (_, KeyCode::Char('r')) => { app.refresh_usage_data(); app.status_msg = "Refreshed".into(); }
        (_, KeyCode::Up)   => { app.report_scroll = app.report_scroll.saturating_add(1); }
        (_, KeyCode::Down) => { app.report_scroll = app.report_scroll.saturating_sub(1); }
        _ => {}
    }
}

fn handle_skills_key(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => { app.active_page = 0; }
        (_, KeyCode::Char('r')) => { app.refresh_skills(); app.status_msg = "Skills refreshed".into(); }
        (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
            if app.skill_selected > 0 { app.skill_selected -= 1; }
        }
        (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
            if !app.skills.is_empty() && app.skill_selected < app.skills.len() - 1 {
                app.skill_selected += 1;
            }
        }
        (_, KeyCode::Enter) => {
            let skill_to_load = app.skills.get(app.skill_selected).map(|s| (s.name.clone(), s.content.clone()));
            if let Some((name, content)) = skill_to_load {
                app.session_mut().input = content;
                let len = app.session().input.len();
                app.session_mut().cursor_pos = len;
                app.active_page = 0;
                app.status_msg = format!("Loaded skill '{}' into chat", name);
            }
        }
        _ => {}
    }
}

async fn handle_settings_key(app: &mut App, key: KeyEvent) {
    if app.settings.editing {
        match key.code {
            KeyCode::Enter => {
                app.settings.editing = false;
                app.save_settings();
            }
            KeyCode::Esc => { app.settings.editing = false; }
            KeyCode::Backspace => { app.settings.fields[app.settings.selected].value.pop(); }
            KeyCode::Char(c) => { app.settings.fields[app.settings.selected].value.push(c); }
            _ => {}
        }
        return;
    }
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('s')) => { app.active_page = 0; }
        (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
            if app.settings.selected > 0 { app.settings.selected -= 1; }
        }
        (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
            if app.settings.selected < app.settings.fields.len() - 1 { app.settings.selected += 1; }
        }
        (_, KeyCode::Enter) | (_, KeyCode::Char('e')) => { app.settings.editing = true; }
        (_, KeyCode::Char('s')) => { app.save_settings(); app.active_page = 0; }
        (_, KeyCode::Char('b')) => { app.backup_config(); }
        (_, KeyCode::Char('r')) => { app.restore_config(); }
        (_, KeyCode::Char('t')) => {
            app.status_msg = "Testing connection...".into();
            app.save_settings();
            app.test_connection().await;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => { app.show_exit_confirmation = true; }
        _ => {}
    }
}

fn ui(f: &mut Frame, app: &App) {
    let size = f.area();
    f.render_widget(Clear, size);
    let bg = Paragraph::new("").style(Style::default().bg(app.theme.bg));
    f.render_widget(bg, size);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(1)])
        .split(size);

    let box_area = outer[0];
    let status_area = outer[1];

    let session = app.session();
    let is_welcome = app.active_page == 0 && session.show_welcome && session.messages.is_empty() && app.sessions.len() == 1;

    let (cursor, pill_area) = if is_welcome {
        // Welcome Screen: Centered Logo + Centered Input Card
        let total_welcome_h = 16u16; // 10 lines for logo & tagline, 1 gap, 5 for input card
        let pad_y = box_area.height.saturating_sub(total_welcome_h) / 2;
        let welcome_h = 10u16.min(box_area.height.saturating_sub(6));
        let welcome_rect = Rect {
            x: box_area.x,
            y: box_area.y + pad_y,
            width: box_area.width,
            height: welcome_h,
        };
        draw_welcome(f, app, welcome_rect);

        let card_w = 82u16.min(box_area.width.saturating_sub(4));
        let card_x = box_area.x + (box_area.width.saturating_sub(card_w)) / 2;
        let card_y = (welcome_rect.y + welcome_h + 1).min(box_area.y + box_area.height.saturating_sub(5));
        let card_rect = Rect {
            x: card_x,
            y: card_y,
            width: card_w,
            height: 5,
        };
        let cur = draw_welcome_input_card(f, app, card_rect);
        (cur, card_rect)
    } else if app.active_page == 0 {
        // Active Chat View: Workspace + Input Docked
        let is_wide = box_area.width >= 100;
        let line_breaks = session.input.chars().filter(|&c| c == '\n').count();
        let input_height: u16 = if session.input.contains('\n') {
            ((line_breaks as u16) + 3).clamp(3, 6)
        } else {
            3
        };
        let gap: u16 = 1;

        if is_wide {
            // Split horizontally: Left Workspace (Min(60)) + Right Sidebar (Length(40))
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(60), Constraint::Length(40)])
                .split(box_area);

            let left_pane = chunks[0];
            let right_pane = chunks[1];

            let session_tab_rect = Rect {
                x: left_pane.x,
                y: left_pane.y,
                width: left_pane.width,
                height: 1,
            };
            draw_session_tabs(f, app, session_tab_rect);

            let chat_h = left_pane.height.saturating_sub(input_height + gap + 1);
            let chat_rect = Rect {
                x: left_pane.x,
                y: left_pane.y + 1,
                width: left_pane.width,
                height: chat_h,
            };
            draw_messages(f, app, session, chat_rect);

            let pill_rect = Rect {
                x: left_pane.x + 1,
                y: left_pane.y + left_pane.height.saturating_sub(input_height + gap),
                width: left_pane.width.saturating_sub(2),
                height: input_height,
            };
            let cur = draw_input(f, app, pill_rect);

            draw_sidebar_inspector(f, app, right_pane);

            (cur, pill_rect)
        } else {
            // Narrow screen: Single column workspace
            let session_tab_rect = Rect {
                x: box_area.x,
                y: box_area.y,
                width: box_area.width,
                height: 1,
            };
            draw_session_tabs(f, app, session_tab_rect);

            let chat_h = box_area.height.saturating_sub(input_height + gap + 1);
            let chat_rect = Rect {
                x: box_area.x,
                y: box_area.y + 1,
                width: box_area.width,
                height: chat_h,
            };
            draw_messages(f, app, session, chat_rect);

            let pill_rect = Rect {
                x: box_area.x + 1,
                y: box_area.y + box_area.height.saturating_sub(input_height + gap),
                width: box_area.width.saturating_sub(2),
                height: input_height,
            };
            let cur = draw_input(f, app, pill_rect);

            (cur, pill_rect)
        }
    } else {
        // Secondary pages keep their own tab bar + content; pill is a hint row.
        let content_y = box_area.y;
        let page_tab_area = Rect {
            x: box_area.x,
            y: content_y,
            width: box_area.width,
            height: 1,
        };
        draw_page_tabs(f, app, page_tab_area);
        let content_h = box_area.height.saturating_sub(3);
        let content_area = Rect {
            x: box_area.x,
            y: content_y + 2,
            width: box_area.width,
            height: content_h,
        };
        match app.active_page {
            1 => draw_monitor_content(f, app, content_area),
            2 => draw_report_content(f, app, content_area),
            3 => draw_skills_content(f, app, content_area),
            4 => draw_settings_content(f, app, content_area),
            _ => {}
        }
        let pill = Rect {
            x: box_area.x,
            y: box_area.y + box_area.height.saturating_sub(1),
            width: box_area.width,
            height: 1,
        };
        let cur = draw_input(f, app, pill);
        (cur, pill)
    };

    draw_status(f, app, status_area);

    if !app.show_exit_confirmation {
        if let Some((cx, cy)) = cursor {
            f.set_cursor_position((cx, cy));
        }
    }

    // Draw autocomplete popup if user is typing a slash command
    if app.active_page == 0 && app.modal_picker == ModalPicker::Off && !app.show_exit_confirmation {
        draw_autocomplete_popup(f, app, pill_area);
        draw_mention_popup(f, app, pill_area);
    }

    // Approval dialog (diff viewer) for the active session
    if app.active_page == 0 && !app.show_exit_confirmation {
        if let Some(t) = app.session().approvals.front() {
            draw_approval(f, app, t, size);
        }
    }

    // Draw modal picker if active (Models or Thinking Mode)
    if app.modal_picker != ModalPicker::Off && !app.show_exit_confirmation {
        draw_modal_picker(f, app, size);
    }

    // Draw exit confirmation modal if requested
    if app.show_exit_confirmation {
        draw_exit_confirmation(f, app, size);
    }
}

fn draw_page_tabs(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut spans = Vec::new();
    spans.push(Span::styled(" [Esc] Back to Chat │ ", Style::default().fg(theme.accent)));
    for (i, name) in PAGE_NAMES.iter().enumerate() {
        let style = if i == app.active_page {
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_muted)
        };
        spans.push(Span::styled(format!("{}:{} ", i + 1, name), style));
        if i < PAGE_NAMES.len() - 1 {
            spans.push(Span::styled("┃ ", Style::default().fg(theme.border)));
        }
    }
    spans.push(Span::styled("│ [Ctrl+1..5 / F1..F5 / ⇆ Tab]", Style::default().fg(theme.text_muted)));
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)), area);
}

fn format_session_tabs_spans<'a>(app: &'a App, width: u16) -> Vec<Span<'a>> {
    let theme = &app.theme;
    let mut left_spans = Vec::new();
    left_spans.push(Span::raw(" "));
    if app.is_pro() {
        left_spans.push(Span::styled("[⭐ PRO] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    }
    for (i, session) in app.sessions.iter().enumerate() {
        let label = short_name(&session.name);
        if i == app.active_session {
            left_spans.push(Span::styled(
                format!("● {} ", label),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ));
        } else {
            left_spans.push(Span::styled(
                format!("{} ", label),
                Style::default().fg(theme.text_muted),
            ));
        }
        if i < app.sessions.len() - 1 || app.sessions.len() < MAX_SESSIONS {
            left_spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
        }
    }
    if app.sessions.len() < MAX_SESSIONS {
        left_spans.push(Span::styled("+ (Ctrl+T) ", Style::default().fg(theme.accent_alt)));
    }

    let right_spans = vec![
        Span::styled("[Tab / ⇆ Switch · Ctrl+W Close] ", Style::default().fg(theme.text_muted)),
    ];

    let left_len: usize = left_spans.iter().map(|s| s.width()).sum();
    let right_len: usize = right_spans.iter().map(|s| s.width()).sum();

    let mut all_spans = left_spans;
    if (width as usize) > left_len + right_len {
        let pad_len = (width as usize) - left_len - right_len;
        all_spans.push(Span::raw(" ".repeat(pad_len)));
        all_spans.extend(right_spans);
    }

    all_spans
}

fn draw_session_tabs(f: &mut Frame, app: &App, area: Rect) {
    let spans = format_session_tabs_spans(app, area.width);
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(app.theme.bg)), area);
}

#[allow(dead_code)]
fn draw_chat_content(f: &mut Frame, app: &App, area: Rect) {
    let session = app.session();
    if session.show_welcome && session.messages.is_empty() {
        draw_welcome(f, app, area);
    } else {
        draw_messages(f, app, session, area);
    }
}

fn draw_welcome(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mut lines: Vec<Line> = Vec::new();
    for row in LOGO {
        lines.push(Line::from(Span::styled(
            *row,
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )));
    }
    for row in WELCOME_LINES {
        if row.contains("Sovereign") {
            let mut spans = vec![
                Span::styled(*row, Style::default().fg(theme.accent_alt).add_modifier(Modifier::BOLD)),
            ];
            if app.is_pro() {
                spans.push(Span::styled("  [⭐ PRO ACTIVE]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            }
            lines.push(Line::from(spans));
        } else if row.starts_with("  •") {
            lines.push(Line::from(Span::styled(*row, Style::default().fg(theme.text_soft))));
        } else {
            lines.push(Line::from(Span::styled(*row, Style::default().fg(theme.text_muted))));
        }
    }

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg));
    f.render_widget(p, area);
}

fn draw_welcome_input_card(f: &mut Frame, app: &App, area: Rect) -> Option<(u16, u16)> {
    let theme = &app.theme;
    let session = app.session();

    let card_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent_alt))
        .style(Style::default().bg(theme.surface_elevated));
    f.render_widget(card_block, area);

    if area.height < 5 || area.width < 10 {
        return None;
    }

    let inner_width = area.width.saturating_sub(4) as usize;

    // Line 1: Input text or placeholder
    let show_placeholder = session.input.is_empty();
    let content_style = if show_placeholder {
        Style::default().fg(theme.text_muted).add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(theme.text_soft)
    };

    let line1 = Line::from(vec![
        if show_placeholder {
            Span::styled("Ask anything... \"fix broken tests\"", content_style)
        } else {
            Span::styled(&*session.input, content_style)
        },
    ]);
    f.render_widget(
        Paragraph::new(line1).style(Style::default().bg(theme.surface_elevated)),
        Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: 1,
        },
    );

    // Line 2: Divider
    let divider = "─".repeat(inner_width);
    let line2 = Line::from(Span::styled(divider, Style::default().fg(theme.border)));
    f.render_widget(
        Paragraph::new(line2).style(Style::default().bg(theme.surface_elevated)),
        Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: 1,
        },
    );

    // Line 3: Footer line
    let mode = app.thinking_mode();
    let model = app.model_name();
    let provider = app.provider_name();

    let left_spans = vec![
        Span::styled("⚡ ", Style::default().fg(theme.warning)),
        Span::styled(format!("{} ", mode), Style::default().fg(theme.text_soft).add_modifier(Modifier::BOLD)),
        Span::styled(" ", Style::default()),
        Span::styled(format!("{} ", model), Style::default().fg(theme.text_soft)),
        Span::styled(" ", Style::default()),
        Span::styled(provider, Style::default().fg(theme.accent_alt)),
    ];

    let right_spans = vec![
        Span::styled("tab", Style::default().fg(theme.accent_alt)),
        Span::styled(" switch agent · ", Style::default().fg(theme.text_muted)),
        Span::styled("ctrl+p", Style::default().fg(theme.accent_alt)),
        Span::styled(" commands", Style::default().fg(theme.text_muted)),
    ];

    let left_len: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let right_len: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
    let pad_len = inner_width.saturating_sub(left_len + right_len);

    let mut footer_spans = left_spans;
    if pad_len > 0 {
        footer_spans.push(Span::raw(" ".repeat(pad_len)));
    }
    footer_spans.extend(right_spans);

    let line3 = Line::from(footer_spans);
    f.render_widget(
        Paragraph::new(line3).style(Style::default().bg(theme.surface_elevated)),
        Rect {
            x: area.x + 2,
            y: area.y + 3,
            width: area.width.saturating_sub(4),
            height: 1,
        },
    );

    let cx = area.x + 2 + session.cursor_pos as u16;
    let cy = area.y + 1;
    Some((cx, cy))
}

fn keywords_for(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" | "rs" => &["fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "use", "mod", "match", "if", "else", "for", "while", "loop", "return", "async", "await", "move", "self", "Self", "const", "static", "where", "type", "crate", "super", "as", "in", "ref", "dyn", "unsafe", "true", "false", "Some", "None", "Ok", "Err"],
        "go" | "golang" => &["func", "package", "import", "var", "const", "type", "struct", "interface", "map", "chan", "go", "defer", "if", "else", "for", "range", "return", "switch", "case", "default", "nil", "true", "false", "err"],
        "ts" | "typescript" | "js" | "javascript" | "tsx" | "jsx" => &["function", "const", "let", "var", "class", "interface", "type", "export", "import", "from", "return", "if", "else", "for", "while", "async", "await", "new", "this", "true", "false", "null", "undefined"],
        "py" | "python" => &["def", "class", "import", "from", "return", "if", "elif", "else", "for", "while", "in", "not", "and", "or", "with", "as", "async", "await", "None", "True", "False", "self", "lambda", "yield"],
        "cs" | "csharp" | "c#" => &["public", "private", "protected", "internal", "class", "struct", "interface", "namespace", "using", "void", "var", "new", "return", "if", "else", "foreach", "for", "async", "await", "static", "readonly", "null", "true", "false", "this"],
        "sh" | "bash" | "shell" | "zsh" => &["if", "then", "fi", "for", "do", "done", "case", "esac", "function", "export", "local", "return", "echo"],
        _ => &[],
    }
}

/// Lightweight syntax highlighter for fenced code (keywords, strings,
/// comments, numbers; diff +/- lines). Not a full parser — just readable.
pub fn highlight_code_line(line: &str, lang: &str, theme: &Theme) -> Vec<Span<'static>> {
    let base = Style::default().fg(theme.text);
    if lang == "diff" || lang == "patch" {
        let st = if line.starts_with('+') { Style::default().fg(theme.success) } else if line.starts_with('-') { Style::default().fg(theme.danger) } else if line.starts_with("@@") { Style::default().fg(theme.accent) } else { base };
        return vec![Span::styled(line.to_string(), st)];
    }
    let kws = keywords_for(lang);
    let comment_start = match lang {
        "py" | "python" | "sh" | "bash" | "shell" | "zsh" | "toml" | "yaml" | "yml" => "#",
        _ => "//",
    };
    let kw_style = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);
    let str_style = Style::default().fg(theme.success);
    let num_style = Style::default().fg(theme.warning);
    let com_style = Style::default().fg(theme.text_muted).add_modifier(Modifier::ITALIC);
    let mut spans = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let flush_word = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if buf.is_empty() {
            return;
        }
        let w = std::mem::take(buf);
        let st = if kws.contains(&w.as_str()) { kw_style } else if w.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) { num_style } else { base };
        spans.push(Span::styled(w, st));
    };
    while i < chars.len() {
        let c = chars[i];
        let rest: String = chars[i..].iter().collect();
        if rest.starts_with(comment_start) {
            flush_word(&mut buf, &mut spans);
            spans.push(Span::styled(rest, com_style));
            return spans;
        }
        if c == '"' || c == '`' || (c == '\'' && !matches!(lang, "rust" | "rs")) {
            flush_word(&mut buf, &mut spans);
            let mut j = i + 1;
            while j < chars.len() && chars[j] != c {
                if chars[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            let end = (j + 1).min(chars.len());
            spans.push(Span::styled(chars[i..end].iter().collect::<String>(), str_style));
            i = end;
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            buf.push(c);
        } else {
            flush_word(&mut buf, &mut spans);
            spans.push(Span::styled(c.to_string(), base));
        }
        i += 1;
    }
    flush_word(&mut buf, &mut spans);
    spans
}

/// Insert pasted text; large pastes are folded into a placeholder token.
fn handle_paste(app: &mut App, text: &str) {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let insert = if text.lines().count() > 8 || text.len() > 2_000 {
        app.session_mut().pastes.push(text.clone());
        let n = app.session().pastes.len();
        paste_tag(n, &text)
    } else {
        text
    };
    let pos = app.session().cursor_pos;
    let valid = if app.session().input.is_char_boundary(pos) { pos } else { app.session().input.len() };
    app.session_mut().input.insert_str(valid, &insert);
    app.session_mut().cursor_pos = valid + insert.len();
    app.session_mut().show_welcome = false;
}

/// `(byte_start, needle)` when the cursor is inside an `@path` token.
pub fn mention_at_cursor(input: &str, cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(input.len());
    if !input.is_char_boundary(cursor) {
        return None;
    }
    let before = &input[..cursor];
    let start = before.rfind(|c: char| c.is_whitespace()).map(|i| i + 1).unwrap_or(0);
    let token = &before[start..];
    let needle = token.strip_prefix('@')?;
    // @dev / @prd / @audit / @fix are protocol commands, not files
    if ["dev", "prd", "audit", "fix"].contains(&needle) {
        return None;
    }
    Some((start, needle.to_string()))
}

pub fn match_files(files: &[String], needle: &str, limit: usize) -> Vec<String> {
    let n = needle.to_lowercase();
    let mut scored: Vec<(usize, &String)> = files
        .iter()
        .filter_map(|f| {
            let lf = f.to_lowercase();
            let name = lf.rsplit('/').next().unwrap_or(&lf).to_string();
            if n.is_empty() {
                Some((3, f))
            } else if name.starts_with(&n) {
                Some((0, f))
            } else if lf.starts_with(&n) {
                Some((1, f))
            } else if lf.contains(&n) {
                Some((2, f))
            } else {
                None
            }
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.len().cmp(&b.1.len())));
    scored.into_iter().take(limit).map(|(_, f)| f.clone()).collect()
}

/// Files mentioned as `@path` that exist in the current directory.
pub fn mentioned_files(input: &str) -> Vec<String> {
    let root = std::env::current_dir().unwrap_or_default();
    input
        .split_whitespace()
        .filter_map(|t| t.strip_prefix('@'))
        .map(|t| t.trim_end_matches([',', '.', ':', ';', ')']))
        .filter(|t| !t.is_empty() && root.join(t).is_file())
        .map(String::from)
        .collect()
}

fn draw_mention_popup(f: &mut Frame, app: &App, input_area: Rect) {
    let session = app.session();
    let Some((_, needle)) = mention_at_cursor(&session.input, session.cursor_pos) else { return };
    let Some((_, files)) = app.file_cache.as_ref() else {
        return;
    };
    let matches = match_files(files, &needle, 8);
    if matches.is_empty() {
        return;
    }
    let width = matches.iter().map(|m| m.chars().count()).max().unwrap_or(10).clamp(20, 70) as u16 + 4;
    let height = matches.len() as u16;
    let rect = Rect { x: input_area.x.saturating_add(2), y: input_area.y.saturating_sub(height), width: width.min(f.area().width), height };
    f.render_widget(Clear, rect);
    let items: Vec<ListItem> = matches
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let sel = i == app.autocomplete_idx % matches.len();
            let style = if sel { Style::default().fg(Color::Black).bg(app.theme.accent).add_modifier(Modifier::BOLD) } else { Style::default().fg(app.theme.text_soft).bg(Color::Rgb(15, 15, 20)) };
            ListItem::new(Line::from(Span::styled(format!("{} @{}", if sel { "▸" } else { " " }, m), style)))
        })
        .collect();
    f.render_widget(List::new(items), rect);
}

/// Colourised unified diff lines.
fn diff_lines<'a>(diff: &'a str, theme: &Theme) -> Vec<Line<'a>> {
    diff.lines()
        .map(|l| {
            let style = if l.starts_with("+++") || l.starts_with("---") {
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
            } else if l.starts_with('+') {
                Style::default().fg(theme.success)
            } else if l.starts_with('-') {
                Style::default().fg(theme.danger)
            } else if l.starts_with("@@") {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.text_soft)
            };
            Line::from(Span::styled(l, style))
        })
        .collect()
}

fn draw_approval(f: &mut Frame, app: &App, ticket: &ApprovalTicket, screen: Rect) {
    let theme = &app.theme;
    let req = &ticket.request;
    let w = screen.width.saturating_sub(6).min(110);
    let diff_len = req.diff.as_ref().map(|d| d.lines().count()).unwrap_or(0) as u16;
    let h = (diff_len + 7).clamp(8, screen.height.saturating_sub(4));
    let rect = Rect { x: screen.x + (screen.width.saturating_sub(w)) / 2, y: screen.y + (screen.height.saturating_sub(h)) / 2, width: w, height: h };
    f.render_widget(Clear, rect);
    let pending = app.session().approvals.len();
    let title = format!(" ⚠ Approval required{} ", if pending > 1 { format!(" (1 of {})", pending) } else { String::new() });
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.warning).add_modifier(Modifier::BOLD))
        .title(title)
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(theme.surface));
    f.render_widget(block, rect);
    let inner = Rect { x: rect.x + 2, y: rect.y + 1, width: rect.width.saturating_sub(4), height: rect.height.saturating_sub(2) };
    let mut lines = vec![
        Line::from(vec![Span::styled(format!("{}: ", req.tool), Style::default().fg(theme.text_muted)), Span::styled(req.summary.clone(), Style::default().fg(theme.text).add_modifier(Modifier::BOLD))]),
        Line::from(""),
    ];
    if let Some(d) = &req.diff {
        lines.extend(diff_lines(d, theme));
    }
    let footer_h = 2u16;
    let body_h = inner.height.saturating_sub(footer_h);
    let max_scroll = (lines.len() as u16).saturating_sub(body_h);
    let scroll = app.approval_scroll.min(max_scroll);
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)).style(Style::default().bg(theme.surface)), Rect { height: body_h, ..inner });
    let footer = Line::from(vec![
        Span::styled("[y] Approve", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("[a] Always (this session)", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("[n] Reject", Style::default().fg(theme.danger).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("↑↓ PgUp/PgDn scroll", Style::default().fg(theme.text_muted)),
    ]);
    f.render_widget(Paragraph::new(vec![Line::from(""), footer]).style(Style::default().bg(theme.surface)), Rect { y: inner.y + body_h, height: footer_h, ..inner });
}

fn draw_exit_confirmation(f: &mut Frame, app: &App, screen: Rect) {
    f.render_widget(
        catui::widgets::ConfirmDialog::new(&app.theme, "Exit Confirmation", "Are you sure you want to quit CACode?")
            .labels("Y: Yes, Exit", "N: Cancel")
            .selected(catui::widgets::ConfirmChoice::Cancel)
            .severity(catui::Status::Warning),
        screen,
    );
}

fn draw_sidebar_inspector(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let status_str = if app.is_thinking { "● Thinking..." } else { "○ Idle" };
    let pro_badge = if app.is_pro() { " [⭐ PRO]" } else { "" };
    let title_span = Span::styled(
        format!(" [ ⚡ RUNTIME{} · {} ] ", pro_badge, status_str),
        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(title_span)
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(theme.bg));
    f.render_widget(block, area);

    if area.width < 10 || area.height < 10 {
        return;
    }

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    let session = app.session();

    // 1. Task Title
    let task_title = session.messages.iter()
        .find(|m| m.role == "user")
        .map(|m| {
            let first_line = m.content.lines().next().unwrap_or(&m.content).trim();
            let clean = first_line.trim_start_matches("> ").trim();
            let max_len = (inner.width as usize).saturating_sub(8);
            if clean.chars().count() > max_len {
                let s: String = clean.chars().take(max_len.saturating_sub(3)).collect();
                format!("{}...", s)
            } else {
                clean.to_string()
            }
        })
        .unwrap_or_else(|| "Active Task".to_string());

    // 2. Tokens & Cost
    let total_tok = session.agent.total_usage.total_tokens.max(session.total_tokens as i64);
    let prompt_tok = session.agent.total_usage.prompt_tokens;
    let compl_tok = session.agent.total_usage.completion_tokens;
    let cost = est_token_cost(app.provider_name(), &app.model_name(), prompt_tok, compl_tok);
    let cost_str = if cost == 0.0 { "$0.00".to_string() } else { format!("${:.2}", cost) };

    // 3. Progress & Steps
    let step_count = session.react_steps.len().clamp(1, 5);
    let current_step = if app.is_thinking {
        step_count.min(5)
    } else if session.messages.is_empty() {
        0
    } else {
        5
    };
    let step_pct = (current_step * 20).min(100);
    let step_bar_w = 8usize;
    let step_filled = ((step_pct as f64 / 100.0) * step_bar_w as f64).round() as usize;

    // 4. Context Window & Visual Meter
    let limit = if session.agent.context_window_limit > 0 { session.agent.context_window_limit } else { 200_000 };
    let pct = ((total_tok as f64 / limit as f64) * 100.0).min(100.0);
    let ctx_bar_w = (inner.width as usize).saturating_sub(10).clamp(8, 20);
    let ctx_filled = ((pct / 100.0) * ctx_bar_w as f64).round() as usize;
    let ctx_color = if pct >= 80.0 {
        theme.danger
    } else if pct >= 50.0 {
        theme.warning
    } else {
        theme.success
    };

    let mut lines = Vec::new();

    // Card 1: TASK
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(theme.border)),
        Span::styled("🎯 TASK ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat((inner.width as usize).saturating_sub(11)), Style::default().fg(theme.border)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled(format!("\"{}\"", task_title), Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
    ]));

    // Card 2: METRICS
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(theme.border)),
        Span::styled("📊 METRICS ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat((inner.width as usize).saturating_sub(14)), Style::default().fg(theme.border)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled("Total: ", Style::default().fg(theme.text_muted)),
        Span::styled(fmt_num(total_tok), Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
        Span::styled("   Cost: ", Style::default().fg(theme.text_muted)),
        Span::styled(cost_str, Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled(format!("In: {:<7} Out: {}", fmt_num(prompt_tok), fmt_num(compl_tok)), Style::default().fg(theme.text_soft)),
    ]));

    // Card 3: PROGRESS & STEPS
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(theme.border)),
        Span::styled("⚡ PROGRESS & STEPS ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat((inner.width as usize).saturating_sub(22)), Style::default().fg(theme.border)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled(format!("Step {}/5 ", current_step), Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
        Span::styled("[", Style::default().fg(theme.border)),
        Span::styled("█".repeat(step_filled), Style::default().fg(theme.accent_alt)),
        Span::styled("░".repeat(step_bar_w.saturating_sub(step_filled)), Style::default().fg(theme.border)),
        Span::styled("] ", Style::default().fg(theme.border)),
        Span::styled(format!("{}%", step_pct), Style::default().fg(theme.text_soft)),
    ]));

    // Card 4: CONTEXT CAPACITY
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(theme.border)),
        Span::styled("🗜️ CONTEXT CAPACITY ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat((inner.width as usize).saturating_sub(23)), Style::default().fg(theme.border)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled(format!("{:.1}k / {}k ", total_tok as f64 / 1000.0, limit / 1000), Style::default().fg(theme.text_soft)),
        Span::styled(format!("({:.1}%)", pct), Style::default().fg(ctx_color).add_modifier(Modifier::BOLD)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled("[", Style::default().fg(theme.border)),
        Span::styled("█".repeat(ctx_filled.min(ctx_bar_w)), Style::default().fg(ctx_color)),
        Span::styled("░".repeat(ctx_bar_w.saturating_sub(ctx_filled)), Style::default().fg(theme.border)),
        Span::styled("]", Style::default().fg(theme.border)),
    ]));

    // Card 5: FAILOVER CHAIN
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(theme.border)),
        Span::styled("🔄 FAILOVER CHAIN ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat((inner.width as usize).saturating_sub(21)), Style::default().fg(theme.border)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("│ ", Style::default().fg(theme.border)),
        Span::styled("● ", Style::default().fg(theme.success)),
        Span::styled(format!("Pri: {:<14}", app.provider_name()), Style::default().fg(theme.text)),
        Span::styled(" [Active]", Style::default().fg(theme.success)),
    ]));
    let fb_chain = &session.agent.fallback_chain;
    if fb_chain.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(theme.border)),
            Span::styled("  No fallbacks configured", Style::default().fg(theme.text_muted)),
        ]));
    } else {
        for (i, fb) in fb_chain.iter().take(3).enumerate() {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(theme.border)),
                Span::styled(format!("  F{}: {:<13}", i + 1, fb.name()), Style::default().fg(theme.text_soft)),
                Span::styled(" [Ready]", Style::default().fg(theme.accent_alt)),
            ]));
        }
    }

    // Card 6: CHECKLIST (TODO)
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(theme.border)),
        Span::styled("📋 CHECKLIST (TODO) ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat((inner.width as usize).saturating_sub(23)), Style::default().fg(theme.border)),
    ]));

    let mut items_to_display: Vec<TodoItem> = session.todos.clone();
    if items_to_display.is_empty() && !session.react_steps.is_empty() {
        for step in &session.react_steps {
            let clean = step.trim();
            let (done, text) = if let Some(s) = clean.strip_prefix("✓ ") {
                (true, s.trim())
            } else if let Some(s) = clean.strip_prefix("● ") {
                (false, s.trim())
            } else if let Some(s) = clean.strip_prefix("✗ ") {
                (false, s.trim())
            } else {
                (false, clean)
            };
            if !text.is_empty() {
                items_to_display.push(TodoItem {
                    text: text.to_string(),
                    done,
                });
            }
        }
    }

    if items_to_display.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(theme.border)),
            Span::styled("• No active tasks", Style::default().fg(theme.text_muted)),
        ]));
    } else {
        let max_items = (inner.height as usize).saturating_sub(20).clamp(2, 5);
        let first_active_idx = items_to_display.iter().position(|t| !t.done);
        let spinner = catui::widgets::Spinner::frame(app.spinner_frame);

        for (i, item) in items_to_display.iter().take(max_items).enumerate() {
            let max_w = (inner.width as usize).saturating_sub(8);
            let display_text = if item.text.chars().count() > max_w {
                let s: String = item.text.chars().take(max_w.saturating_sub(3)).collect();
                format!("{}...", s)
            } else {
                item.text.clone()
            };

            if item.done {
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.border)),
                    Span::styled("[✓] ", Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
                    Span::styled(display_text, Style::default().fg(theme.text_muted).add_modifier(Modifier::CROSSED_OUT)),
                ]));
            } else if Some(i) == first_active_idx {
                let bullet = if app.is_thinking {
                    format!("[{}] ", spinner)
                } else {
                    "[●] ".to_string()
                };
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.border)),
                    Span::styled(bullet, Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)),
                    Span::styled(display_text, Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.border)),
                    Span::styled("[ ] ", Style::default().fg(theme.text_muted)),
                    Span::styled(display_text, Style::default().fg(theme.text_soft)),
                ]));
            }
        }
    }

    // Card 7: Footer
    lines.push(Line::from(vec![
        Span::styled("└─ ", Style::default().fg(theme.border)),
        Span::styled("^C", Style::default().fg(theme.accent_alt)),
        Span::styled(" cancel · ", Style::default().fg(theme.text_muted)),
        Span::styled("Tab", Style::default().fg(theme.accent_alt)),
        Span::styled(" tab · ", Style::default().fg(theme.text_muted)),
        Span::styled("Esc", Style::default().fg(theme.accent_alt)),
        Span::styled(" exit", Style::default().fg(theme.text_muted)),
    ]));

    let p = Paragraph::new(lines).style(Style::default().bg(theme.bg));
    f.render_widget(p, inner);
}

fn draw_messages(f: &mut Frame, app: &App, session: &ChatSession, area: Rect) {
    let theme = &app.theme;
    let mut lines: Vec<Line> = Vec::new();

    for msg in &session.messages {
        match msg.role.as_str() {
            "user" if msg.content.contains("[Pasted #") => {
                for (idx, line) in msg.content.lines().enumerate() {
                    let lead = if idx == 0 { "● " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(lead, Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                        Span::styled(line.to_string(), Style::default().fg(theme.text).add_modifier(Modifier::BOLD)),
                    ]));
                }
                lines.push(Line::from(""));
            }
            "user" => {
                let unescaped = unescape_newlines(&msg.content);
                for (idx, line) in unescaped.lines().enumerate() {
                    let mut spans = Vec::new();
                    if idx == 0 {
                        spans.push(Span::styled("● ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)));
                    } else {
                        spans.push(Span::raw("  "));
                    }
                    spans.extend(render_styled_line(line, Style::default().fg(theme.text).add_modifier(Modifier::BOLD)));
                    lines.push(Line::from(spans));
                }
                if unescaped.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                    ]));
                }
                lines.push(Line::from(""));
            }
            "tool_start" => {
                let unescaped = unescape_newlines(&msg.content);
                let parts: Vec<&str> = unescaped.splitn(2, ' ').collect();
                let tool_name = parts[0];
                let tool_args = if parts.len() > 1 { parts[1] } else { "" };
                lines.push(Line::from(vec![
                    Span::styled("  ┌─ ⚙️ Action: ", Style::default().fg(theme.accent_alt)),
                    Span::styled(tool_name.to_string(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                    Span::styled(" ──────────────────────────", Style::default().fg(theme.border)),
                ]));
                if !tool_args.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("  │  Args: ", Style::default().fg(theme.text_muted)),
                        Span::styled(tool_args.to_string(), Style::default().fg(theme.text_soft)),
                    ]));
                }
                lines.push(Line::from(Span::styled("  └──────────────────────────────────────", Style::default().fg(theme.border))));
            }
            "tool_result" => {
                let unescaped = unescape_newlines(&msg.content);
                let mut msg_lines = unescaped.lines();
                if let Some(badge) = msg_lines.next() {
                    lines.push(Line::from(vec![
                        Span::styled("  ↳ ", Style::default().fg(theme.border)),
                        Span::styled(format!("[✓ Result: {}]", badge), Style::default().fg(theme.success).add_modifier(Modifier::BOLD)),
                    ]));
                }
                for l in msg_lines {
                    let mut spans = vec![
                        Span::styled("    │ ", Style::default().fg(theme.border)),
                    ];
                    spans.extend(render_styled_line(l, Style::default().fg(theme.text_soft)));
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
            }
            "tool_result_error" => {
                let unescaped = unescape_newlines(&msg.content);
                let mut msg_lines = unescaped.lines();
                if let Some(badge) = msg_lines.next() {
                    lines.push(Line::from(vec![
                        Span::styled("  ↳ ", Style::default().fg(theme.border)),
                        Span::styled(format!("[✗ Error: {}]", badge), Style::default().fg(theme.danger).add_modifier(Modifier::BOLD)),
                    ]));
                }
                for l in msg_lines {
                    lines.push(Line::from(vec![
                        Span::styled("    │ ", Style::default().fg(theme.border)),
                        Span::styled(l.to_string(), Style::default().fg(theme.danger)),
                    ]));
                }
                lines.push(Line::from(""));
            }
            "assistant" => {
                let unescaped = unescape_newlines(&msg.content);
                let mut in_code_block = false;
                let mut code_lang = String::new();
                for line in unescaped.lines() {
                    if line.trim_start().starts_with("```") {
                        if !in_code_block {
                            in_code_block = true;
                            let lang = line.trim_start().trim_start_matches('`').trim();
                            code_lang = lang.to_lowercase();
                            let lang_tag = if lang.is_empty() { "code" } else { lang };
                            lines.push(Line::from(vec![
                                Span::styled("  ╭── ", Style::default().fg(theme.accent_alt)),
                                Span::styled(lang_tag.to_string(), Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)),
                                Span::styled(" ────────────────────", Style::default().fg(theme.accent_alt)),
                            ]));
                        } else {
                            in_code_block = false;
                            lines.push(Line::from(Span::styled("  ╰────────────────────────────", Style::default().fg(theme.accent_alt))));
                        }
                    } else if in_code_block {
                        let mut spans = vec![Span::styled("  │ ", Style::default().fg(theme.accent_alt))];
                        spans.extend(highlight_code_line(line, &code_lang, theme));
                        lines.push(Line::from(spans));
                    } else {
                        let styled_spans = render_styled_line(line, Style::default().fg(theme.text_soft));
                        lines.push(Line::from(styled_spans));
                    }
                }
                if let Some(ref u) = msg.tokens {
                    let total_str = fmt_num(u.total_tokens);
                    let in_str = fmt_num(u.prompt_tokens);
                    let out_str = fmt_num(u.completion_tokens);
                    let sec = msg.duration_ms.unwrap_or(0) as f64 / 1000.0;
                    let tag = format!(
                        "[⚡ {} tokens ({} in / {} out) • {:.1}s]",
                        total_str, in_str, out_str, sec
                    );
                    lines.push(Line::from(Span::styled(
                        tag,
                        Style::default().fg(theme.text_muted).add_modifier(Modifier::DIM),
                    )));
                }
                lines.push(Line::from(""));
            }
            _ => {
                let unescaped = unescape_newlines(&msg.content);
                for line in unescaped.lines() {
                    let styled_spans = render_styled_line(line, Style::default().fg(theme.text_muted).add_modifier(Modifier::ITALIC));
                    lines.push(Line::from(styled_spans));
                }
                lines.push(Line::from(""));
            }
        }
    }

    let total = lines.len() as u16;
    let visible = area.height;
    let max_scroll = total.saturating_sub(visible);
    let scroll = max_scroll.saturating_sub(session.scroll_offset);

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .style(Style::default().bg(theme.bg));
    f.render_widget(p, area);
}

fn draw_autocomplete_popup(f: &mut Frame, app: &App, input_area: Rect) {
    let input_str = app.session().input.trim();
    if !input_str.starts_with('/') { return; }

    let needle = input_str;
    let matches: Vec<(&str, &str)> = SLASH_COMMANDS
        .iter()
        .filter(|(cmd, _)| cmd.starts_with(needle) || needle == "/" || cmd.contains(needle.trim_start_matches('/')))
        .copied()
        .collect();

    if matches.is_empty() { return; }

    let count = matches.len().min(8);
    let width = 56u16;
    let height = count as u16;
    let x = input_area.x.saturating_add(2);
    let y = input_area.y.saturating_sub(height);

    let popup_rect = Rect { x, y, width, height };
    f.render_widget(Clear, popup_rect);

    let items: Vec<ListItem> = matches
        .iter()
        .take(count)
        .enumerate()
        .map(|(i, (cmd, desc))| {
            let sel = i == (app.autocomplete_idx % matches.len());
            let (pfx, style) = if sel {
                ("▸ ", Style::default().fg(Color::Black).bg(app.theme.accent).add_modifier(Modifier::BOLD))
            } else {
                ("  ", Style::default().fg(app.theme.text_soft).bg(Color::Rgb(15, 15, 20)))
            };
            let line = Line::from(vec![
                Span::styled(pfx, style),
                Span::styled(format!("{:<12}", cmd), style),
                Span::styled(format!(" {}", desc), if sel { style } else { Style::default().fg(app.theme.text_muted).bg(Color::Rgb(15, 15, 20)) }),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).style(Style::default().bg(Color::Rgb(15, 15, 20)));
    f.render_widget(list, popup_rect);
}

fn draw_modal_picker(f: &mut Frame, app: &App, screen: Rect) {
    let (title, count, items) = match app.modal_picker {
        ModalPicker::Models => {
            let items: Vec<Line> = app.models_cache.iter().enumerate().map(|(i, m)| {
                let sel = i == app.modal_idx;
                let is_current = m == &app.model_name();
                let pfx = if sel { "▸ " } else { "  " };
                let tag = if is_current { " [active]" } else { "" };
                let style = if sel {
                    Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)
                } else if is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(app.theme.text_soft)
                };
                Line::from(vec![
                    Span::styled(pfx, style),
                    Span::styled(m.as_str(), style),
                    Span::styled(tag, Style::default().fg(Color::Green)),
                ])
            }).collect();
            (" Select AI Model (/models) ", app.models_cache.len(), items)
        }
        ModalPicker::Thinking => {
            let items: Vec<Line> = THINKING_MODES.iter().enumerate().map(|(i, (mode, desc))| {
                let sel = i == app.modal_idx;
                let is_current = *mode == app.thinking_mode();
                let pfx = if sel { "▸ " } else { "  " };
                let tag = if is_current { " [active]" } else { "" };
                let style = if sel {
                    Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)
                } else if is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(app.theme.text_soft)
                };
                Line::from(vec![
                    Span::styled(pfx, style),
                    Span::styled(format!("{:<14}", mode), style),
                    Span::styled(format!(" — {}", desc), if sel { style } else { Style::default().fg(app.theme.text_muted) }),
                    Span::styled(tag, Style::default().fg(Color::Green)),
                ])
            }).collect();
            (" Select Thinking / Reasoning Mode (/mode) ", THINKING_MODES.len(), items)
        }
        ModalPicker::Off => return,
    };

    let width = 64u16.min(screen.width.saturating_sub(4));
    let height = (count as u16 + 5).min(screen.height.saturating_sub(4));
    let x = (screen.width.saturating_sub(width)) / 2;
    let y = (screen.height.saturating_sub(height)) / 2;
    let area = Rect { x, y, width, height };

    f.render_widget(Clear, area);
    let bg_rect = Paragraph::new("").style(Style::default().bg(Color::Rgb(15, 15, 20)));
    f.render_widget(bg_rect, area);

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };

    let header = Paragraph::new(Line::from(Span::styled(
        title,
        Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(header, Rect { height: 1, ..inner });

    let list_area = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: inner.height.saturating_sub(3),
    };

    let list_items: Vec<ListItem> = items.into_iter().map(ListItem::new).collect();
    f.render_widget(List::new(list_items), list_area);

    let help = Paragraph::new(Line::from(vec![
        Span::styled("↑↓", Style::default().fg(app.theme.accent)),
        Span::styled(" navigate  ", Style::default().fg(app.theme.text_muted)),
        Span::styled("Enter", Style::default().fg(app.theme.accent)),
        Span::styled(" select  ", Style::default().fg(app.theme.text_muted)),
        Span::styled("Esc", Style::default().fg(app.theme.accent)),
        Span::styled(" cancel", Style::default().fg(app.theme.text_muted)),
    ]));
    f.render_widget(help, Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner });
}

fn draw_monitor_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" ◉ ", Style::default().fg(app.theme.accent)),
        Span::styled("Token Usage Monitor", Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("   (press ", Style::default().fg(app.theme.text_muted)),
        Span::styled("r", Style::default().fg(app.theme.accent)),
        Span::styled(" to refresh, Esc to back)", Style::default().fg(app.theme.text_muted)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(app.theme.bg)), Rect { height: 1, ..area });

    if app.usage_rows.is_empty() {
        let msg = "No usage data recorded yet. Start monitor proxy via `cacode monitor --endpoint <url>`";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(app.theme.text_muted))).style(Style::default().bg(app.theme.bg));
        f.render_widget(p, Rect { y: area.y + 2, height: 1, ..area });
        return;
    }

    let table_area = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2),
    };

    let header = Row::new(vec!["Timestamp", "Model", "Tokens", "Cost ($)"])
        .style(Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.usage_rows.iter().map(|r| {
        let ts = if r.timestamp.len() > 19 { &r.timestamp[..19] } else { &r.timestamp };
        Row::new(vec![
            ts.to_string(),
            r.model.clone(),
            format!("{}", r.total_tokens),
            format!("{:.6}", r.est_cost),
        ])
        .style(Style::default().fg(app.theme.text_soft))
    }).collect();

    let widths = [
        Constraint::Length(20),
        Constraint::Min(20),
        Constraint::Length(10),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::default().bg(app.theme.bg));

    f.render_widget(table, table_area);
}

fn draw_report_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" 📊 ", Style::default().fg(app.theme.accent)),
        Span::styled("Usage Report (per-model aggregation)", Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("   (press ", Style::default().fg(app.theme.text_muted)),
        Span::styled("r", Style::default().fg(app.theme.accent)),
        Span::styled(" to refresh, Esc to back)", Style::default().fg(app.theme.text_muted)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(app.theme.bg)), Rect { height: 1, ..area });

    if app.report_rows.is_empty() {
        let msg = "No usage data to aggregate.";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(app.theme.text_muted))).style(Style::default().bg(app.theme.bg));
        f.render_widget(p, Rect { y: area.y + 2, height: 1, ..area });
        return;
    }

    let header = Row::new(vec!["Model", "Calls", "Prompt", "Completion", "Total", "Cost ($)"])
        .style(Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.report_rows.iter().map(|r| {
        Row::new(vec![
            r.model.clone(),
            format!("{}", r.calls),
            format!("{}", r.prompt_tokens),
            format!("{}", r.completion_tokens),
            format!("{}", r.total_tokens),
            format!("{:.6}", r.est_cost),
        ])
        .style(Style::default().fg(app.theme.text_soft))
    }).collect();

    let total_calls: i64 = app.report_rows.iter().map(|r| r.calls).sum();
    let total_prompt: i64 = app.report_rows.iter().map(|r| r.prompt_tokens).sum();
    let total_comp: i64 = app.report_rows.iter().map(|r| r.completion_tokens).sum();
    let total_tok: i64 = app.report_rows.iter().map(|r| r.total_tokens).sum();
    let total_cost: f64 = app.report_rows.iter().map(|r| r.est_cost).sum();

    let mut all_rows = rows;
    all_rows.push(
        Row::new(vec![
            "TOTAL".to_string(),
            format!("{}", total_calls),
            format!("{}", total_prompt),
            format!("{}", total_comp),
            format!("{}", total_tok),
            format!("{:.6}", total_cost),
        ])
        .style(Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD))
    );

    let widths = [
        Constraint::Min(20),
        Constraint::Length(7),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(12),
    ];

    let table_h = 2u16 + all_rows.len() as u16 + 1;
    let table = Table::new(all_rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::default().bg(app.theme.bg));

    let table_rect = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: table_h.min(area.height.saturating_sub(2)),
    };
    f.render_widget(table, table_rect);

    // Daily Heatmap (Traffic & Activity)
    if !app.daily_rows.is_empty() {
        let heat_title_y = table_rect.y + table_rect.height + 1;
        if heat_title_y + 3 < area.y + area.height {
            let heat_header = Line::from(vec![
                Span::styled(" 📈 ", Style::default().fg(app.theme.accent)),
                Span::styled("Daily Traffic Heatmap & Top Model Analytics (last 14 days)", Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
            ]);
            f.render_widget(Paragraph::new(heat_header).style(Style::default().bg(app.theme.bg)), Rect {
                x: area.x,
                y: heat_title_y,
                width: area.width,
                height: 1,
            });

            let top_model_name = app.report_rows.first().map(|r| r.model.as_str()).unwrap_or("-");
            let top_model_share = if !app.report_rows.is_empty() {
                let total_tok: i64 = app.report_rows.iter().map(|r| r.total_tokens).sum();
                if total_tok > 0 {
                    (app.report_rows[0].total_tokens as f64 / total_tok as f64) * 100.0
                } else {
                    0.0
                }
            } else {
                0.0
            };
            let top_msg = format!(
                "Most Used Model: {} ({:.1}% of total tokens)  │  Total Sessions/Days Logged: {}",
                top_model_name, top_model_share, app.daily_rows.len()
            );
            f.render_widget(Paragraph::new(Span::styled(top_msg, Style::default().fg(Color::Yellow))).style(Style::default().bg(app.theme.bg)), Rect {
                x: area.x,
                y: heat_title_y + 1,
                width: area.width,
                height: 1,
            });

            for (i, row) in app.daily_rows.iter().enumerate() {
                let bar_y = heat_title_y + 2 + i as u16;
                if bar_y >= area.y + area.height {
                    break;
                }
                let bar_line = Line::from(vec![
                    Span::styled(format!("{:<10}", row.day), Style::default().fg(app.theme.text_soft)),
                    Span::styled(format!("{:>5}", row.calls), Style::default().fg(app.theme.text_muted)),
                    Span::styled(" calls  │  ", Style::default().fg(app.theme.text_muted)),
                    Span::styled(format!("{:>10}", row.tokens), Style::default().fg(Color::White)),
                    Span::styled(" tokens │  ", Style::default().fg(app.theme.text_muted)),
                    Span::styled(format!("${:.4}  ", row.cost), Style::default().fg(Color::Green)),
                    Span::styled(row.bar.as_str(), Style::default().fg(app.theme.accent)),
                ]);
                f.render_widget(Paragraph::new(bar_line).style(Style::default().bg(app.theme.bg)), Rect {
                    x: area.x,
                    y: bar_y,
                    width: area.width,
                    height: 1,
                });
            }
        }
    }
}

fn draw_skills_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" ⚡ ", Style::default().fg(app.theme.accent)),
        Span::styled("Skills Library (~/.cacode/skills/)", Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled("   (Enter load into chat, Esc back)", Style::default().fg(app.theme.text_muted)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(app.theme.bg)), Rect { height: 1, ..area });

    if app.skills.is_empty() {
        let msg = "No skills saved yet. Use `/skill add <name>` in chat to save prompt templates.";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(app.theme.text_muted))).style(Style::default().bg(app.theme.bg));
        f.render_widget(p, Rect { y: area.y + 2, height: 1, ..area });
        return;
    }

    let table_area = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2),
    };

    let header = Row::new(vec!["", "Skill Name", "Created", "Description"])
        .style(Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.skills.iter().enumerate().map(|(i, s)| {
        let sel = i == app.skill_selected;
        let pfx = if sel { "▸ " } else { "  " };
        let style = if sel { Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD) } else { Style::default().fg(app.theme.text_soft) };

        Row::new(vec![
            pfx.to_string(),
            s.name.clone(),
            s.created.clone(),
            s.description.clone(),
        ])
        .style(style)
    }).collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Length(25),
        Constraint::Length(12),
        Constraint::Min(30),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::default().bg(app.theme.bg));

    f.render_widget(table, table_area);
}

fn draw_settings_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 4 { return; }

    let title = Line::from(vec![
        Span::styled(" ⚙  BYOK Settings & Configuration ", Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
        Span::styled(" (Esc to back)", Style::default().fg(app.theme.text_muted)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(app.theme.bg)), Rect { height: 1, ..area });

    let list_area = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(5),
    };

    let items: Vec<ListItem> = app.settings.fields.iter().enumerate().map(|(i, field)| {
        let sel = i == app.settings.selected;
        let display = if app.settings.editing && sel {
            if field.is_secret && !field.value.is_empty() {
                format!("{}█", "*".repeat(field.value.len()))
            } else {
                format!("{}█", field.value)
            }
        } else if field.is_secret {
            CaCodeConfig::mask(&field.value)
        } else if field.value.is_empty() {
            "(not set)".into()
        } else {
            field.value.clone()
        };

        let pfx = if sel { if app.settings.editing { "✎ " } else { "▸ " } } else { "  " };
        let style = if sel {
            if app.settings.editing { Style::default().fg(Color::Yellow) }
            else { Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD) }
        } else { Style::default().fg(app.theme.text) };

        ListItem::new(Line::from(vec![
            Span::styled(pfx, style),
            Span::styled(format!("{:20}", field.label), style),
            Span::styled(" │ ", Style::default().fg(app.theme.text_muted)),
            Span::styled(display, style),
        ]))
    }).collect();

    f.render_widget(List::new(items).style(Style::default().bg(app.theme.bg)), list_area);

    let help_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(2),
        width: area.width,
        height: 2,
    };
    let help = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("↑↓", Style::default().fg(app.theme.accent)),
            Span::styled(" navigate  ", Style::default().fg(app.theme.text_muted)),
            Span::styled("Enter", Style::default().fg(app.theme.accent)),
            Span::styled(" edit  ", Style::default().fg(app.theme.text_muted)),
            Span::styled("s", Style::default().fg(app.theme.accent)),
            Span::styled(" save  ", Style::default().fg(app.theme.text_muted)),
            Span::styled("b", Style::default().fg(app.theme.accent)),
            Span::styled(" backup  ", Style::default().fg(app.theme.text_muted)),
            Span::styled("r", Style::default().fg(app.theme.accent)),
            Span::styled(" restore  ", Style::default().fg(app.theme.text_muted)),
            Span::styled("t", Style::default().fg(app.theme.accent)),
            Span::styled(" test  ", Style::default().fg(app.theme.text_muted)),
            Span::styled("Esc", Style::default().fg(app.theme.accent)),
            Span::styled(" back to chat", Style::default().fg(app.theme.text_muted)),
        ]),
        Line::from(Span::styled(
            if app.status_msg.is_empty() { "" } else { &app.status_msg },
            Style::default().fg(Color::Green),
        )),
    ]).style(Style::default().bg(app.theme.bg));
    f.render_widget(help, help_area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) -> Option<(u16, u16)> {
    let theme = &app.theme;
    if app.active_page != 0 {
        let hint = match app.active_page {
            1 => "↑↓ scroll • r refresh • ⇆ Tab/F1-F5 switch page • Esc back to chat",
            2 => "↑↓ scroll • r refresh • ⇆ Tab/F1-F5 switch page • Esc back to chat",
            3 => "↑↓ select • Enter load skill • ⇆ Tab/F1-F5 switch page • Esc back to chat",
            4 => "↑↓ navigate • Enter edit • s save • ⇆ Tab/F1-F5 switch page • Esc back to chat",
            _ => "↑↓ navigate • ⇆ Tab/F1-F5 switch page • Esc back to chat",
        };
        let line = Line::from(Span::styled(hint, Style::default().fg(theme.text_muted)));
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.bg)),
            area,
        );
        return None;
    }

    let session = app.session();
    let text_col = if app.is_thinking { theme.accent_alt } else { theme.text_soft };
    let has_image = detect_image_reference(&session.input).is_some();

    if app.is_thinking && !session.input.is_empty() {
        // Mid-turn steering: user is actively typing a steering instruction.
        let steer_label = "⚡ steer ";
        let (row, col) = get_cursor_coords(&session.input, session.cursor_pos);
        let inner_y = area.y + 1;
        let inner_h = area.height.saturating_sub(2);
        let scroll_top = if row >= inner_h as usize { row - inner_h as usize + 1 } else { 0 };

        let mut steer_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.surface_elevated));

        if has_image {
            steer_block = steer_block.title(
                Line::from(vec![
                    Span::styled(" [🖼️ Image] ", Style::default().fg(theme.accent_alt).add_modifier(Modifier::BOLD))
                ]).alignment(Alignment::Right)
            );
        }
        f.render_widget(steer_block, area);

        let lines_raw: Vec<&str> = session.input.split('\n').collect();
        let mut rendered_lines = Vec::new();
        for (idx, line_str) in lines_raw.iter().enumerate().skip(scroll_top).take(inner_h as usize) {
            let mut spans = Vec::new();
            if idx == 0 {
                spans.push(Span::styled(steer_label, Style::default().fg(theme.warning).add_modifier(Modifier::BOLD)));
            } else {
                spans.push(Span::raw(" ".repeat(steer_label.chars().count())));
            }
            spans.push(Span::styled(*line_str, Style::default().fg(theme.text_soft)));
            rendered_lines.push(Line::from(spans));
        }

        f.render_widget(
            Paragraph::new(rendered_lines).style(Style::default().bg(theme.surface_elevated)),
            Rect {
                x: area.x + 2,
                y: inner_y,
                width: area.width.saturating_sub(4),
                height: inner_h,
            },
        );

        let pfx_w = steer_label.chars().count() as u16;
        let cx = area.x + 2 + pfx_w + col as u16;
        let cy = inner_y + (row - scroll_top) as u16;
        Some((cx.min(area.x + area.width.saturating_sub(2)), cy))
    } else if app.is_thinking {
        // Rotating braille spinner without leading '>'
        let spinner = catui::widgets::Spinner::frame(app.spinner_frame);
        let line = Line::from(vec![
            Span::styled(
                format!("{} ", spinner),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Thinking... · Type to steer · Ctrl+C to cancel",
                Style::default().fg(theme.text_muted).add_modifier(Modifier::ITALIC),
            ),
        ]);
        let thinking_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.surface_elevated));
        f.render_widget(thinking_block, area);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(theme.surface_elevated)),
            Rect {
                x: area.x + 2,
                y: area.y + 1,
                width: area.width.saturating_sub(4),
                height: 1,
            },
        );
        None
    } else {
        // Idle input pill with placeholder when empty — NO leading '> ' prompt.
        let (row, col) = get_cursor_coords(&session.input, session.cursor_pos);
        let show_placeholder = session.input.is_empty();
        let content_style = if show_placeholder {
            Style::default().fg(theme.text_muted).add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(text_col)
        };

        let mut input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent_alt))
            .style(Style::default().bg(theme.surface_elevated));

        if has_image {
            input_block = input_block.title(
                Line::from(vec![
                    Span::styled(" [🖼️ Image] ", Style::default().fg(theme.accent_alt).add_modifier(Modifier::BOLD))
                ]).alignment(Alignment::Right)
            );
        }
        f.render_widget(input_block, area);

        let inner_y = area.y + 1;
        let inner_h = area.height.saturating_sub(2);
        let scroll_top = if row >= inner_h as usize { row - inner_h as usize + 1 } else { 0 };

        let rendered_lines: Vec<Line> = if show_placeholder {
            vec![Line::from(Span::styled("Ask anything...", content_style))]
        } else {
            session.input.split('\n')
                .skip(scroll_top)
                .take(inner_h as usize)
                .map(|line_str| Line::from(Span::styled(line_str, content_style)))
                .collect()
        };

        f.render_widget(
            Paragraph::new(rendered_lines).style(Style::default().bg(theme.surface_elevated)),
            Rect {
                x: area.x + 2,
                y: inner_y,
                width: area.width.saturating_sub(4),
                height: inner_h,
            },
        );

        let cx = area.x + 2 + col as u16;
        let cy = inner_y + (row - scroll_top) as u16;
        Some((cx.min(area.x + area.width.saturating_sub(2)), cy))
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let provider = app.provider_name();
    let model = app.model_name();

    let (status_dot, status_label, status_color) = if app.is_thinking {
        ("●", "Build", theme.warning)
    } else {
        ("●", "Idle", theme.success)
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd_str = if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = cwd.strip_prefix(&home) {
            format!("~/{}", rel.display())
        } else {
            cwd.display().to_string()
        }
    } else {
        cwd.display().to_string()
    };

    let mut left_spans = vec![
        Span::styled(format!(" {} ", cwd_str), Style::default().fg(theme.text_soft)),
        Span::styled("· ", Style::default().fg(theme.text_muted)),
        Span::styled(format!("{} ", status_dot), Style::default().fg(status_color)),
        Span::styled(format!("{} ", status_label), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled("· ", Style::default().fg(theme.text_muted)),
        Span::styled(format!("{} ", model), Style::default().fg(theme.text_soft).add_modifier(Modifier::BOLD)),
        Span::styled("· ", Style::default().fg(theme.text_muted)),
        Span::styled(format!("{} ", provider), Style::default().fg(theme.accent_alt)),
    ];

    if app.is_pro() {
        left_spans.push(Span::styled("· ", Style::default().fg(theme.text_muted)));
        left_spans.push(Span::styled("[⭐ PRO] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    }

    if app.sessions.len() > 1 {
        let busy = app.sessions.iter().filter(|s| s.is_busy()).count();
        left_spans.push(Span::styled("· ", Style::default().fg(theme.text_muted)));
        left_spans.push(Span::styled(
            format!("agent {}/{}{} ", app.active_session + 1, app.sessions.len(), if busy > 0 { format!(" ({} running)", busy) } else { String::new() }),
            Style::default().fg(theme.text_muted),
        ));
    }

    // live turn telemetry: elapsed, active tool, streaming speed, permission mode
    left_spans.push(Span::styled("· ", Style::default().fg(theme.text_muted)));
    left_spans.push(Span::styled(format!("🔐 {} ", app.session().agent.permission_mode.label()), Style::default().fg(theme.text_muted)));
    if let Some(rt) = &app.session().turn {
        let spin = catui::widgets::Spinner::frame(app.spinner_frame);
        let secs = rt.started.elapsed().as_secs_f64();
        let mut t = format!("{} {:.0}s", spin, secs);
        if let Some((tool, since)) = &rt.active_tool {
            t.push_str(&format!(" ⚙ {} {:.0}s", tool, since.elapsed().as_secs_f64()));
        } else if rt.streamed_chars > 0 && secs > 0.0 {
            t.push_str(&format!(" ~{:.0} tok/s", (rt.streamed_chars as f64 / 3.5) / secs));
        }
        left_spans.push(Span::styled("· ", Style::default().fg(theme.text_muted)));
        left_spans.push(Span::styled(format!("{} ", t), Style::default().fg(theme.warning)));
    }

    if !app.status_msg.is_empty() {
        left_spans.push(Span::styled("· ", Style::default().fg(theme.text_muted)));
        left_spans.push(Span::styled(format!("{} ", app.status_msg), Style::default().fg(theme.accent_alt)));
    }

    let right_spans = vec![
        Span::styled("/status", Style::default().fg(theme.accent_alt)),
        Span::styled("  ·  ", Style::default().fg(theme.text_muted)),
        Span::styled(format!("v{} ", CA_VERSION), Style::default().fg(theme.text_muted)),
    ];

    let left_len: usize = left_spans.iter().map(|s| s.content.chars().count()).sum();
    let right_len: usize = right_spans.iter().map(|s| s.content.chars().count()).sum();
    let pad_len = (area.width as usize).saturating_sub(left_len + right_len);

    let mut all_spans = left_spans;
    if pad_len > 0 {
        all_spans.push(Span::raw(" ".repeat(pad_len)));
    }
    all_spans.extend(right_spans);

    let line = Line::from(all_spans);
    let p = Paragraph::new(line).style(Style::default().bg(theme.bg));
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unescape_newlines() {
        let input = "hello\\nworld\\r\\nfoo\\tbar\\\"baz\\\"";
        let output = unescape_newlines(input);
        assert_eq!(output, "hello\nworld\nfoo    bar\"baz\"");
    }

    #[test]
    fn test_format_tool_action() {
        let (verb, target) = format_tool_action("read_file", r#"{"path":"src/main.rs"}"#);
        assert_eq!(verb, "Reading file");
        assert_eq!(target, "src/main.rs");

        let (verb, cmd) = format_tool_action("exec_command", r#"{"command":"cargo test"}"#);
        assert_eq!(verb, "Running command");
        assert_eq!(cmd, "cargo test");

        let (verb, pat) = format_tool_action("search_files", r#"{"pattern":"TODO"}"#);
        assert_eq!(verb, "Searching files");
        assert_eq!(pat, "TODO");
    }

    #[test]
    fn test_est_token_cost() {
        let zen_cost = est_token_cost("Antigravity Zen", "deepseek-v4-flash-free", 1000, 1000);
        assert_eq!(zen_cost, 0.0);

        let paid_cost = est_token_cost("OpenAI", "gpt-4o", 1_000_000, 1_000_000);
        assert!((paid_cost - 12.5).abs() < 1e-4);
        // model-aware: Claude Sonnet is not billed at gpt-4o rates
        let claude = est_token_cost("Anthropic", "claude-sonnet-4-20250514", 1_000_000, 1_000_000);
        assert!((claude - 18.0).abs() < 1e-4);
    }

    #[test]
    fn test_exit_confirmation_flow() {
        let cfg = CaCodeConfig::default();
        let mut app = App::new(cfg);
        assert!(!app.show_exit_confirmation);
        assert!(!app.should_quit);

        // Open exit confirmation
        app.show_exit_confirmation = true;

        // Cancel with 'n'
        let cancel_key = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        let captured = handle_global_key(&mut app, cancel_key);
        assert!(captured);
        assert!(!app.show_exit_confirmation);
        assert!(!app.should_quit);

        // Open again and confirm with 'y'
        app.show_exit_confirmation = true;
        let confirm_key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        let captured = handle_global_key(&mut app, confirm_key);
        assert!(captured);
        assert!(app.should_quit);
    }

    #[test]
    fn test_react_steps_tracking() {
        let cfg = CaCodeConfig::default();
        let mut session = ChatSession::new("Test".into(), &cfg);
        assert!(session.react_steps.is_empty());

        session.react_steps.push("● read_file src/lib.rs".to_string());
        assert_eq!(session.react_steps.len(), 1);

        // Simulate tool completion
        if let Some(last) = session.react_steps.last_mut() {
            *last = format!("✓ {}", last.trim_start_matches("● ").trim());
        }
        assert_eq!(session.react_steps[0], "✓ read_file src/lib.rs");
    }

    #[test]
    fn test_tab_page_navigation() {
        let cfg = CaCodeConfig::default();
        let mut app = App::new(cfg);
        assert_eq!(app.active_page, 0);

        // F2 switches to page 1 (Monitor)
        let f2 = KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);
        assert!(handle_global_key(&mut app, f2));
        assert_eq!(app.active_page, 1);

        // F3 switches to page 2 (Report)
        let f3 = KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE);
        assert!(handle_global_key(&mut app, f3));
        assert_eq!(app.active_page, 2);

        // In secondary page, Right or Tab moves to page 3 (Skills)
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert!(handle_global_key(&mut app, right));
        assert_eq!(app.active_page, 3);

        // 'l' moves to page 4 (Settings)
        let l_key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(handle_global_key(&mut app, l_key));
        assert_eq!(app.active_page, 4);

        // 'h' moves back to page 3 (Skills)
        let h_key = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(handle_global_key(&mut app, h_key));
        assert_eq!(app.active_page, 3);

        // Esc returns straight to chat (page 0)
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(handle_global_key(&mut app, esc));
        assert_eq!(app.active_page, 0);
    }

    #[test]
    fn test_session_tabs_bar_formatting() {
        let cfg = CaCodeConfig::default();
        let mut app = App::new(cfg);
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.active_session, 0);

        // 1. Initial single session formatting
        let spans = format_session_tabs_spans(&app, 100);
        let text_content: String = spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(text_content.contains("● Term 1"));
        assert!(text_content.contains("│"));
        assert!(text_content.contains("+ (Ctrl+T)"));
        assert!(text_content.contains("[Tab / ⇆ Switch · Ctrl+W Close]"));

        // Verify active session styling
        let active_span = spans.iter().find(|s| s.content.contains("Term 1")).unwrap();
        assert_eq!(active_span.style.fg, Some(app.theme.accent));
        assert!(active_span.style.add_modifier.contains(Modifier::BOLD));

        // 2. Multi-session formatting
        app.new_session(); // creates Term 2, active_session = 1
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active_session, 1);

        let spans2 = format_session_tabs_spans(&app, 120);
        let s1_span = spans2.iter().find(|s| s.content.contains("Term 1")).unwrap();
        assert_eq!(s1_span.style.fg, Some(app.theme.text_muted));
        assert!(!s1_span.style.add_modifier.contains(Modifier::BOLD));

        let s2_span = spans2.iter().find(|s| s.content.contains("Term 2")).unwrap();
        assert_eq!(s2_span.style.fg, Some(app.theme.accent));
        assert!(s2_span.style.add_modifier.contains(Modifier::BOLD));

        // 3. Tab cycling
        app.next_session();
        assert_eq!(app.active_session, 0);
        app.prev_session();
        assert_eq!(app.active_session, 1);

        // 4. Max sessions boundary
        for _ in 2..MAX_SESSIONS {
            app.new_session();
        }
        assert_eq!(app.sessions.len(), MAX_SESSIONS);

        let spans_max = format_session_tabs_spans(&app, 160);
        let max_text: String = spans_max.iter().map(|s| s.content.as_ref()).collect();
        assert!(!max_text.contains("+ (Ctrl+T)"));

        // 5. Session closing & minimum boundary
        while app.sessions.len() > 1 {
            app.close_session();
        }
        assert_eq!(app.sessions.len(), 1);
        app.close_session(); // should not close last session
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.status_msg, "Cannot close last session");
    }

    #[test]
    fn test_multiline_cursor_calculation() {
        let input = "hello\nworld\nfoo";

        // Cursor at 0 ('h' in "hello") -> line 0, col 0
        assert_eq!(get_cursor_coords(input, 0), (0, 0));
        // Cursor at 3 ('l' in "hello") -> line 0, col 3
        assert_eq!(get_cursor_coords(input, 3), (0, 3));
        // Cursor at 5 (end of "hello", right before '\n') -> line 0, col 5
        assert_eq!(get_cursor_coords(input, 5), (0, 5));
        // Cursor at 6 ('w' in "world") -> line 1, col 0
        assert_eq!(get_cursor_coords(input, 6), (1, 0));
        // Cursor at 9 ('l' in "world") -> line 1, col 3
        assert_eq!(get_cursor_coords(input, 9), (1, 3));
        // Cursor at 11 (end of "world") -> line 1, col 5
        assert_eq!(get_cursor_coords(input, 11), (1, 5));
        // Cursor at 12 ('f' in "foo") -> line 2, col 0
        assert_eq!(get_cursor_coords(input, 12), (2, 0));
        // Cursor at 15 (end of "foo") -> line 2, col 3
        assert_eq!(get_cursor_coords(input, 15), (2, 3));

        // Trailing newline
        let input2 = "abc\n";
        assert_eq!(get_cursor_coords(input2, 3), (0, 3));
        assert_eq!(get_cursor_coords(input2, 4), (1, 0));
    }

    #[test]
    fn test_image_and_link_styling() {
        // Image detection helper
        assert_eq!(detect_image_reference("look at preview.png now"), Some("preview.png".to_string()));
        assert_eq!(detect_image_reference("check ![Architecture](docs/arch.svg)"), Some("docs/arch.svg".to_string()));
        assert_eq!(detect_image_reference("@img:screenshot.jpg is attached"), Some("screenshot.jpg".to_string()));
        assert_eq!(detect_image_reference("plain text without images"), None);

        // Link & Image styled line rendering
        let line = "Visit https://github.com/cecepazhar/cacode and see ![Mockup](assets/ui.png) or git@github.com:repo.git";
        let spans = render_styled_line(line, Style::default().fg(Color::White));

        // Verify link styling: Cyan + UNDERLINED
        let link_span = spans.iter().find(|s| s.content.contains("https://github.com/cecepazhar/cacode")).unwrap();
        assert_eq!(link_span.style.fg, Some(Color::Cyan));
        assert!(link_span.style.add_modifier.contains(Modifier::UNDERLINED));

        let git_span = spans.iter().find(|s| s.content.contains("git@github.com:repo.git")).unwrap();
        assert_eq!(git_span.style.fg, Some(Color::Cyan));
        assert!(git_span.style.add_modifier.contains(Modifier::UNDERLINED));

        // Verify image badge: [🖼️ Image: ui.png] in Cyan + BOLD
        let img_span = spans.iter().find(|s| s.content.contains("[🖼️ Image: ui.png]")).unwrap();
        assert_eq!(img_span.style.fg, Some(Color::Cyan));
        assert!(img_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn test_todo_extraction_and_strikethrough() {
        let cfg = CaCodeConfig::default();
        let mut session = ChatSession::new("TodoSession".into(), &cfg);
        assert!(session.todos.is_empty());

        let markdown = "Here is our plan:\n- [ ] First pending task\n- [x] Completed task\n* [X] Another finished task\n1. [ ] Numbered pending";
        session.update_todos_from_text(markdown);

        assert_eq!(session.todos.len(), 4);
        assert_eq!(session.todos[0], TodoItem { text: "First pending task".into(), done: false });
        assert_eq!(session.todos[1], TodoItem { text: "Completed task".into(), done: true });
        assert_eq!(session.todos[2], TodoItem { text: "Another finished task".into(), done: true });
        assert_eq!(session.todos[3], TodoItem { text: "Numbered pending".into(), done: false });

        // Update existing task state
        session.update_todos_from_text("- [x] First pending task");
        assert!(session.todos[0].done);

        // Verify fallback to react_steps auto-generation
        let mut session_steps = ChatSession::new("StepSession".into(), &cfg);
        session_steps.react_steps.push("✓ read_file src/lib.rs".into());
        session_steps.react_steps.push("● write_file src/main.rs".into());
        session_steps.sync_todos_from_steps();
        assert_eq!(session_steps.todos.len(), 2);
        assert_eq!(session_steps.todos[0], TodoItem { text: "read_file src/lib.rs".into(), done: true });
        assert_eq!(session_steps.todos[1], TodoItem { text: "write_file src/main.rs".into(), done: false });

        // Verify strikethrough styling logic
        let completed_style = Style::default().fg(Color::Rgb(100, 116, 139)).add_modifier(Modifier::CROSSED_OUT);
        assert_eq!(completed_style.fg, Some(Color::Rgb(100, 116, 139))); // #64748b
        assert!(completed_style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn test_theme_preset_palette_generation() {
        // TokyoNight
        let tokyo = Theme::from_name("tokyo");
        assert_eq!(tokyo.preset, ThemePreset::TokyoNight);
        assert_eq!(tokyo.bg, Color::Rgb(26, 27, 38)); // #1a1b26
        assert_eq!(tokyo.accent, Color::Rgb(122, 162, 247)); // #7aa2f7
        assert_eq!(tokyo.accent_alt, Color::Rgb(187, 154, 247)); // #bb9af7
        assert_eq!(tokyo.success, Color::Rgb(158, 206, 106)); // #9ece6a

        // Nord
        let nord = Theme::from_name("nord");
        assert_eq!(nord.preset, ThemePreset::Nord);
        assert_eq!(nord.bg, Color::Rgb(46, 52, 64)); // #2e3440
        assert_eq!(nord.accent, Color::Rgb(136, 192, 208)); // #88c0d0
        assert_eq!(nord.text, Color::Rgb(236, 239, 244)); // #eceff4

        // OneDark
        let onedark = Theme::from_name("onedark");
        assert_eq!(onedark.preset, ThemePreset::OneDark);
        assert_eq!(onedark.bg, Color::Rgb(33, 37, 43)); // #21252b
        assert_eq!(onedark.accent, Color::Rgb(97, 175, 239)); // #61afef
        assert_eq!(onedark.warning, Color::Rgb(229, 192, 123)); // #e5c07b

        // HighContrast
        let contrast = Theme::from_name("contrast");
        assert_eq!(contrast.preset, ThemePreset::HighContrast);
        assert_eq!(contrast.bg, Color::Rgb(0, 0, 0));
        assert_eq!(contrast.accent, Color::Rgb(250, 204, 21)); // #facc15
        assert_eq!(contrast.text, Color::Rgb(255, 255, 255));

        // Light
        let light = Theme::from_name("light");
        assert_eq!(light.preset, ThemePreset::CaLight);
        assert_eq!(light.bg, Color::Rgb(248, 250, 252)); // #f8fafc
        // CATUI: `light` now maps to CAUI's light tokens; accent = CAUI brand.
        assert_eq!(light.accent, catui::tokens::DEFAULT_BRAND.color());
        assert_eq!(light.text, Color::Rgb(15, 23, 42)); // #0f172a

        // Classic
        let classic = Theme::from_name("classic");
        assert_eq!(classic.preset, ThemePreset::Midnight);
        assert_eq!(classic.bg, Color::Rgb(5, 8, 15)); // #05080f
        assert_eq!(classic.accent, Color::Rgb(59, 130, 246)); // #3b82f6
    }

    #[test]
    fn test_skill_wizard_question_and_answer_state_machine() {
        let cfg = CaCodeConfig::default();
        let mut session = ChatSession::new("WizardTest".into(), &cfg);
        assert_eq!(session.wizard_state, WizardState::None);

        // Step 1: User triggers @prd
        session.wizard_state = WizardState::PrdStep1ProjectName;

        // User enters project idea
        let project_name = "Sistem POS Kasir Bengkel Motor Offline-First".to_string();
        session.wizard_state = WizardState::PrdStep2Stack { project_name: project_name.clone() };

        // Step 2: User selects recommended option 1
        let stack_choice = "1";
        let resolved_stack = match stack_choice {
            "1" => "Rust + Tauri v2 (High-performance, sovereign desktop, zero-crash)".to_string(),
            "2" => "Go + HTMX / Templ (Lightweight single binary)".to_string(),
            "3" => "TypeScript + SvelteKit / CAUI (Modern reactive UI)".to_string(),
            "4" => "Python + FastAPI (Rapid API prototype)".to_string(),
            other => other.to_string(),
        };
        session.wizard_state = WizardState::PrdStep3Structure {
            project_name: project_name.clone(),
            stack: resolved_stack.clone(),
        };

        // Step 3: User selects recommended structure option 1
        let structure_choice = "1";
        let resolved_structure = match structure_choice {
            "1" => "Modular Workspace: Core Library + CLI + TUI + Local SQLite".to_string(),
            "2" => "Clean Architecture: Domain + UseCase + Infrastructure + Presentation".to_string(),
            "3" => "Microservices: API Gateway + Business Services + Background Worker".to_string(),
            other => other.to_string(),
        };
        session.wizard_state = WizardState::None;

        assert_eq!(project_name, "Sistem POS Kasir Bengkel Motor Offline-First");
        assert!(resolved_stack.contains("Tauri v2"));
        assert!(resolved_structure.contains("Modular Workspace"));
        assert_eq!(session.wizard_state, WizardState::None);
    }

    #[test]
    fn test_pro_key_detection_and_badge_formatting() {
        let mut cfg = CaCodeConfig::default();
        assert!(!cfg.is_pro());

        let app = App::new(cfg.clone());
        assert!(!app.is_pro());
        let spans_free = format_session_tabs_spans(&app, 120);
        let free_text: String = spans_free.iter().map(|s| s.content.as_ref()).collect();
        assert!(!free_text.contains("[⭐ PRO]"));

        // Activate Pro key
        cfg.pro_license_key = Some("CACD99-ABC123-6".to_string());
        assert!(cfg.is_pro());

        let app_pro = App::new(cfg);
        assert!(app_pro.is_pro());
        let spans_pro = format_session_tabs_spans(&app_pro, 120);
        let pro_text: String = spans_pro.iter().map(|s| s.content.as_ref()).collect();
        assert!(pro_text.contains("[⭐ PRO]"));
    }

    #[test]
    fn test_custom_provider_and_fallback_chain_config() {
        let mut cfg = CaCodeConfig::default();
        cfg.custom_providers.push(CustomProviderConfig {
            id: "groq".to_string(),
            name: "Groq Cloud".to_string(),
            base_url: "https://api.groq.com/openai/v1".to_string(),
            api_key: "gsk_test".to_string(),
            model: "llama-3.3-70b-versatile".to_string(),
        });

        // Test provider string resolution
        let resolved_zen = resolve_provider_string("zen", &cfg).unwrap();
        assert_eq!(resolved_zen.name(), "Antigravity Zen");

        let resolved_custom = resolve_provider_string("groq", &cfg).unwrap();
        assert_eq!(resolved_custom.name(), "Groq Cloud");
        assert_eq!(resolved_custom.model(), "llama-3.3-70b-versatile");

        // Test fallback chain building
        cfg.fallback_1 = Some("groq".to_string());
        cfg.fallback_2 = Some("ollama".to_string());
        cfg.fallback_3 = Some("9router".to_string());

        let chain = build_fallback_chain(&cfg);
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].name(), "Groq Cloud");
        assert_eq!(chain[1].name(), "Ollama");
        assert_eq!(chain[2].name(), "9Router");
    }
}
#[cfg(test)]
mod audit_probe {
    #[test]
    fn probe_utf8_slice_panic() {
        let s = format!("{{\"qq\":\"{}\"}}", "é".repeat(40));
        let r = std::panic::catch_unwind(|| super::format_tool_action("custom_tool", &s));
        println!("UTF8 PANIC = {}", r.is_err());
        assert!(r.is_ok(), "multi-byte tool args must never panic the TUI");
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal OpenAI-compatible mock: serves `replies` in order (last repeats).
    async fn mock(replies: Vec<serde_json::Value>, delay_ms: u64) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let replies = std::sync::Arc::new(replies);
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let replies = replies.clone();
                let n = n.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1 << 20];
                    let mut got = 0;
                    loop {
                        let k = s.read(&mut buf[got..]).await.unwrap_or(0);
                        got += k;
                        let t = String::from_utf8_lossy(&buf[..got]).to_string();
                        if let Some(h) = t.find("\r\n\r\n") {
                            let cl = t.lines().find(|l| l.to_lowercase().starts_with("content-length:")).and_then(|l| l[15..].trim().parse::<usize>().ok()).unwrap_or(0);
                            if got >= h + 4 + cl {
                                break;
                            }
                        }
                        if k == 0 {
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let i = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = replies.get(i).or(replies.last()).unwrap().to_string();
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://127.0.0.1:{}", port)
    }

    fn text(t: &str) -> serde_json::Value {
        serde_json::json!({"choices": [{"message": {"role": "assistant", "content": t}}], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})
    }
    fn tool(name: &str, args: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})
    }

    fn point(app: &mut App, idx: usize, url: &str, dir: &std::path::Path, mode: PermissionMode) {
        let a = &mut app.sessions[idx].agent;
        a.provider = Provider::Custom { name: "mock".into(), base_url: url.into(), api_key: String::new(), model: "m".into() };
        a.fallback_chain.clear();
        a.workspace_root = Some(dir.to_path_buf());
        a.permission_mode = mode;
        a.inject_repo_context = false;
        a.record_usage = false;
        a.autosave = false;
        a.stream = false;
    }

    async fn pump_until(app: &mut App, mut done: impl FnMut(&App) -> bool) {
        let t0 = Instant::now();
        while !done(app) {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for UI state");
            app.drain_agent_events();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn tab_switch_mid_turn_does_not_wedge_other_tabs() {
        let d = tempfile::tempdir().unwrap();
        let url = mock(vec![text("slow answer")], 600).await;
        let mut app = App::new(CaCodeConfig::default());
        app.new_session();
        point(&mut app, 0, &url, d.path(), PermissionMode::Yolo);
        point(&mut app, 1, &url, d.path(), PermissionMode::Yolo);
        app.active_session = 0;
        app.start_turn("first".into());
        assert!(app.sessions[0].is_busy());
        app.next_session(); // user switches to Term 2 while Term 1 is thinking
        app.drain_agent_events();
        assert!(!app.is_thinking, "Term 2 is idle and must accept input");
        app.start_turn("second".into());
        assert!(app.sessions[1].is_busy(), "Term 2 starts its own turn instead of steering a dead channel");
        pump_until(&mut app, |a| !a.sessions[0].is_busy() && !a.sessions[1].is_busy()).await;
        for s in &app.sessions {
            assert_eq!(s.agent.history.last().unwrap().content, "slow answer");
        }
    }

    #[tokio::test]
    async fn approval_dialog_applies_edit_only_after_yes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
        let url = mock(
            vec![tool("read_file", serde_json::json!({"path": "a.txt"})), tool("edit_file", serde_json::json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"})), text("done")],
            0,
        )
        .await;
        let mut app = App::new(CaCodeConfig::default());
        point(&mut app, 0, &url, d.path(), PermissionMode::Ask);
        app.start_turn("edit it".into());
        pump_until(&mut app, |a| !a.sessions[0].approvals.is_empty()).await;
        let t = app.sessions[0].approvals.front().unwrap();
        assert!(t.request.diff.as_ref().unwrap().contains("+bye"));
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "hello\n", "nothing written before approval");
        // keys go to the dialog
        assert!(handle_global_key(&mut app, KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)));
        pump_until(&mut app, |a| !a.sessions[0].is_busy()).await;
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "bye\n");
    }

    #[tokio::test]
    async fn ctrl_c_keeps_history_of_cancelled_turn() {
        let d = tempfile::tempdir().unwrap();
        let url = mock(vec![tool("bash", serde_json::json!({"command": "sleep 20"}))], 0).await;
        let mut app = App::new(CaCodeConfig::default());
        point(&mut app, 0, &url, d.path(), PermissionMode::Yolo);
        app.start_turn("long".into());
        pump_until(&mut app, |a| a.sessions[0].messages.iter().any(|m| m.role == "tool_start")).await;
        app.cancel_turn();
        pump_until(&mut app, |a| !a.sessions[0].is_busy()).await;
        let h = &app.sessions[0].agent.history;
        assert_eq!(h.first().unwrap().content, "long", "user prompt kept");
        assert!(h.iter().any(|m| m.role == "tool"), "tool result (cancelled) kept so the next request is valid");
    }

    #[tokio::test]
    async fn compress_runs_in_background() {
        let d = tempfile::tempdir().unwrap();
        let url = mock(vec![text("[COMPACTED CONTEXT SUMMARY] s")], 500).await;
        let mut app = App::new(CaCodeConfig::default());
        point(&mut app, 0, &url, d.path(), PermissionMode::Yolo);
        for i in 0..6 {
            app.sessions[0].agent.history.push(Message::user(format!("q{i}")));
            app.sessions[0].agent.history.push(Message::assistant(format!("a{i}")));
        }
        let t0 = Instant::now();
        app.start_compress();
        assert!(t0.elapsed() < Duration::from_millis(100), "start_compress must return immediately");
        app.drain_agent_events();
        assert!(app.is_thinking);
        pump_until(&mut app, |a| a.bg_busy == 0).await;
        assert!(app.sessions[0].agent.history[0].content.contains("COMPACTED"));
    }

    #[test]
    fn paste_folding_and_expansion() {
        let mut app = App::new(CaCodeConfig::default());
        let big: String = (0..20).map(|i| format!("line {i}\n")).collect();
        handle_paste(&mut app, &big);
        let input = app.session().input.clone();
        assert_eq!(input, "[Pasted #1: 20 lines]");
        assert_eq!(app.session().expand_pastes(&input), big.replace("\r\n", "\n"));
        handle_paste(&mut app, " small");
        assert!(app.session().input.ends_with(" small"));
    }

    #[test]
    fn mention_detection_and_matching() {
        assert_eq!(mention_at_cursor("look at @src/ma", 15), Some((8, "src/ma".into())));
        assert_eq!(mention_at_cursor("@dev", 4), None);
        assert_eq!(mention_at_cursor("no mention", 10), None);
        let files = vec!["src/main.rs".to_string(), "crates/x/src/main.rs".to_string(), "README.md".to_string()];
        assert_eq!(match_files(&files, "main", 8)[0], "src/main.rs");
        assert_eq!(match_files(&files, "read", 8), vec!["README.md".to_string()]);
    }

    #[test]
    fn highlighter_marks_keywords_strings_comments() {
        let th = Theme::default();
        let spans = highlight_code_line("let s = \"hi\"; // note", "rust", &th);
        let kw = spans.iter().find(|s| s.content == "let").unwrap();
        assert_eq!(kw.style.fg, Some(th.accent));
        assert!(spans.iter().any(|s| s.content == "\"hi\"" && s.style.fg == Some(th.success)));
        assert!(spans.iter().any(|s| s.content.starts_with("// note")));
        let lifetimes = highlight_code_line("fn f<'a>(x: &'a str) {}", "rust", &th);
        assert!(lifetimes.iter().any(|s| s.content == "str"), "rust lifetimes are not strings");
    }

    #[test]
    fn ui_renders_approval_dialog_with_diff() {
        use ratatui::backend::TestBackend;
        let mut app = App::new(CaCodeConfig::default());
        let (t, _rx) = ApprovalTicket::new(cacode_core::events::ApprovalRequest {
            id: 1,
            tool: "edit_file".into(),
            summary: "Edit src/main.rs".into(),
            diff: Some("--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-hello\n+bye\n".into()),
        });
        app.sessions[0].approvals.push_back(t);
        app.sessions[0].show_welcome = false;
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| ui(f, &app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("Approval required"), "dialog title missing");
        assert!(text.contains("Edit src/main.rs"));
        assert!(text.contains("+bye") && text.contains("-hello"));
        assert!(text.contains("[y] Approve") && text.contains("[n] Reject"));
    }

    #[tokio::test]
    async fn read_state_survives_across_turns() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "one\n").unwrap();
        let url = mock(
            vec![
                tool("read_file", serde_json::json!({"path": "a.txt"})),
                text("read it"),
                tool("edit_file", serde_json::json!({"path": "a.txt", "old_string": "one", "new_string": "two"})),
                text("edited"),
            ],
            0,
        )
        .await;
        let mut app = App::new(CaCodeConfig::default());
        point(&mut app, 0, &url, d.path(), PermissionMode::Yolo);
        app.start_turn("read a.txt".into());
        pump_until(&mut app, |a| !a.sessions[0].is_busy()).await;
        app.start_turn("now change it".into());
        pump_until(&mut app, |a| !a.sessions[0].is_busy()).await;
        let errs: Vec<&Message> = app.sessions[0].agent.history.iter().filter(|m| m.is_error).collect();
        assert!(errs.is_empty(), "edit in turn 2 must see the read from turn 1: {:?}", errs);
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "two\n");
    }
}

