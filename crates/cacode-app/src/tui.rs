use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Row, Table, Widget, Wrap},
    Frame, Terminal,
};
use rusqlite::Connection;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::env;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use cacode_core::commands::chat::{AgentEvent, Provider, ReActAgent, TokenUsage};
use crate::config::CaCodeConfig;

const BRAND: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::White;
const GRAY: Color = Color::Rgb(180, 180, 190);
const BG_BLACK: Color = Color::Rgb(0, 0, 0);

// Minimalist single-column layout: near-black background, blue/cyan accent.
// Codex-style navy header panel and ? shortcuts overlay are intentionally omitted.
const BG_DARK: Color = Color::Rgb(5, 8, 15); // near-black (#05080f)
const BG_PILL: Color = Color::Rgb(10, 16, 32); // slightly lighter pill background
const ACCENT: Color = Color::Rgb(59, 130, 246); // #3b82f6 blue accent
const ACCENT_DIM: Color = Color::Rgb(96, 165, 250); // brighter blue/cyan accent
const TEXT_SOFT: Color = Color::Rgb(226, 232, 240); // soft lavender/white
const MUTED: Color = Color::Rgb(100, 116, 139); // muted slate

#[allow(dead_code)]
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
    "  Sovereign Rust Development Agent  v1.0.2",
    "",
    "  • ReAct Agent Loop with deterministic local tool execution",
    "  • Multi-provider BYOK & Free: Antigravity Zen, Ollama, 9Router, OpenAI, Anthropic",
    "  • Local token usage monitor & cost estimator, Skill System",
    "  • Zero telemetry — your keys, your machine, your data",
    "",
    "  Type a message to chat, or '/' for command autocomplete",
    "    /providers   /provider   /models   /mode   /settings   /skills",
    "",
];

const PAGE_NAMES: &[&str] = &["Chat", "Monitor", "Report", "Skills", "Settings"];
const MAX_SESSIONS: usize = 9;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/models", "Interactive AI model picker"),
    ("/providers", "Probe & display local & free providers (Ollama, Zen)"),
    ("/provider", "Switch active provider (/provider <name>)"),
    ("/mode", "Select thinking level: Fast, Analytical, Deep"),
    ("/settings", "Open BYOK settings & provider configuration"),
    ("/monitor", "View real-time token telemetry & proxy log"),
    ("/report", "View token usage summary, heatmap & model analytics"),
    ("/skills", "Manage custom skills & reusable prompts"),
    ("/dev", "⚡ Feature development & implementation protocol"),
    ("/prd", "📋 Product Requirements Document drafting workflow"),
    ("/audit", "🛡️ Security & architecture whitebox audit"),
    ("/refact", "🧹 Code refactoring, simplification & cleanup"),
    ("/update", "📦 Dependency bump, migration & semantic version update"),
    ("/fix", "🐞 Systematic 4-phase bugfix protocol"),
    ("/bughunt", "🎯 Deep bug hunt (boundary fuzzing, concurrency & panics)"),
    ("/test", "🧪 TDD test authoring (unit & integration tests)"),
    ("/qa", "✅ Quality Assurance dual-check sign-off"),
    ("/compress", "🗜️ Compress conversation history (token optimization)"),
    ("/context", "📊 Show active context window tokens & usage limit"),
    ("/status", "Show active provider, model, tokens & system stats"),
    ("/sessions", "List all active chat sessions"),
    ("/rename", "Rename current chat session (/rename <name>)"),
    ("/clear", "Clear current conversation memory"),
    ("/export", "Export conversation transcript to Markdown file"),
    ("/version", "Show CACode version (v1.0.2)"),
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

const THINKING_FRAMES: &[(&str, &str)] = &[
    ("⠋", "( •‿• ) [CA] reasoning..."),
    ("⠙", "( ⚆_⚆ ) [CA] inspecting..."),
    ("⠹", "( ◉_◉ ) [CA] analyzing..."),
    ("⠸", "( ◕‿◕ ) [CA] synthesizing..."),
    ("⠼", "( ˘ᴗ˘ ) [CA] optimizing..."),
    ("⠴", "( ◉‿◉ ) [CA] formulating..."),
    ("⠦", "( •‿• ) [CA] verifying..."),
    ("⠧", "( ⚆_⚆ ) [CA] concluding..."),
];

#[allow(dead_code)]
struct BorderlessLayout {
    divider_y: Option<u16>,
}

#[allow(dead_code)]
impl BorderlessLayout {
    fn new() -> Self { Self { divider_y: None } }
    fn with_divider(mut self, y: u16) -> Self { self.divider_y = Some(y); self }
    fn inner(&self, area: Rect) -> Rect { area }
}

impl Widget for BorderlessLayout {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 2 || area.height < 2 { return; }
        let x1 = area.x;
        let x2 = area.x + area.width.saturating_sub(1);
        let y1 = area.y;
        let style = Style::default().fg(DIM);

        // Render subtle horizontal divider above input prompt
        if let Some(dy) = self.divider_y {
            let div_y = y1 + dy;
            if div_y > y1 && div_y < y1 + area.height {
                for x in x1..=x2 {
                    buf.set_string(x, div_y, "─", style);
                }
            }
        }
    }
}

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

struct ChatSession {
    name: String,
    messages: Vec<ChatMessage>,
    agent: ReActAgent,
    input: String,
    cursor_pos: usize,
    scroll_offset: u16,
    total_tokens: u64,
    show_welcome: bool,
}

impl ChatSession {
    fn new(name: String, config: &CaCodeConfig) -> Self {
        let mut agent = ReActAgent::new();
        agent.user_name = config.user_name.clone();
        agent.persona = config.system_persona.clone();
        agent.thinking_mode = config.thinking_mode.clone();
        Self {
            name,
            messages: Vec::new(),
            agent,
            input: String::new(),
            cursor_pos: 0,
            scroll_offset: 0,
            total_tokens: 0,
            show_welcome: true,
        }
    }
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
    usage_rows: Vec<UsageRow>,
    report_rows: Vec<ReportRow>,
    daily_rows: Vec<DailyTrafficRow>,
    skills: Vec<SkillItem>,
    skill_selected: usize,
    monitor_scroll: u16,
    report_scroll: u16,
    spinner_frame: usize,
    // Autocomplete state
    autocomplete_idx: usize,
    // Modal picker state
    modal_picker: ModalPicker,
    modal_idx: usize,
    models_cache: Vec<String>,
    // Non-blocking agent runtime state
    agent_events_rx: Option<mpsc::UnboundedReceiver<AgentEvent>>,
    steering_tx: Option<mpsc::UnboundedSender<String>>,
    cancel_token: Option<CancellationToken>,
    agent_turn_session: usize,
}

impl App {
    fn new(config: CaCodeConfig) -> Self {
        let settings = Self::build_settings(&config);
        let session = ChatSession::new("Session 1".into(), &config);
        let mut app = Self {
            active_page: 0,
            sessions: vec![session],
            active_session: 0,
            config,
            settings,
            status_msg: String::new(),
            is_thinking: false,
            should_quit: false,
            usage_rows: Vec::new(),
            report_rows: Vec::new(),
            daily_rows: Vec::new(),
            skills: Vec::new(),
            skill_selected: 0,
            monitor_scroll: 0,
            report_scroll: 0,
            spinner_frame: 0,
            autocomplete_idx: 0,
            modal_picker: ModalPicker::Off,
            modal_idx: 0,
            models_cache: Vec::new(),
            agent_events_rx: None,
            steering_tx: None,
            cancel_token: None,
            agent_turn_session: 0,
        };
        app.refresh_skills();
        app.refresh_models_cache();
        app
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
    /// Returns immediately; progress is delivered via `agent_events_rx`.
    fn start_turn(&mut self, prompt: String) {
        if self.is_thinking {
            // Already busy: forward it as a mid-turn steering instruction.
            self.steer(prompt);
            return;
        }

        let session_idx = self.active_session;
        let session_name = self.sessions[session_idx].name.clone();
        let user_agent = self.sessions[session_idx].agent.clone();
        let config = self.config.clone();

        // Apply session-level settings to the worker's copy so the background
        // task matches the live UI state.
        let mut agent = user_agent;
        agent.user_name = config.user_name.clone();
        agent.persona = config.system_persona.clone();
        agent.thinking_mode = config.thinking_mode.clone();

        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        let cancel = CancellationToken::new();

        self.agent_events_rx = Some(ev_rx);
        self.steering_tx = Some(steer_tx);
        self.cancel_token = Some(cancel.clone());
        self.agent_turn_session = session_idx;

        tokio::spawn(async move {
            agent.run_turn(prompt, ev_tx, steer_rx, cancel).await;
        });

        self.is_thinking = true;
        self.status_msg = format!("{} — reasoning…", session_name);
    }

    /// Forward a mid-run steering instruction to the active worker.
    fn steer(&mut self, instruction: String) {
        if let Some(tx) = &self.steering_tx {
            let _ = tx.send(instruction);
        }
    }

    /// Cancel the in-flight LLM call / tool execution immediately.
    fn cancel_turn(&mut self) {
        if let Some(tok) = self.cancel_token.clone() {
            tok.cancel();
        }
    }

    /// Drains all pending agent events into the owning session.
    /// Invoked by the event loop on every tick (non-blocking).
    fn drain_agent_events(&mut self) {
        // Take ownership of the receiver so we can clear it without a
        // conflict with the mutable borrow; put it back after processing.
        let mut rx = match self.agent_events_rx.take() {
            Some(rx) => rx,
            None => return,
        };

        let mut finished = false;
        while let Ok(ev) = rx.try_recv() {
            let target = if self.agent_turn_session < self.sessions.len() {
                self.agent_turn_session
            } else {
                self.active_session
            };

            match ev {
                AgentEvent::Thinking(stage) => {
                    self.sessions[target].show_welcome = false;
                    if self.active_session == target {
                        self.status_msg = format!("{} — {}", self.sessions[target].name, stage);
                    }
                }
                AgentEvent::AssistantChunk(text) => {
                    self.sessions[target]
                        .messages
                        .push(ChatMessage::plain("assistant", text));
                    self.sessions[target].scroll_offset = 0;
                }
                AgentEvent::ToolStart { tool, args } => {
                    let short_args = if args.len() > 120 {
                        format!("{}…", &args[..120])
                    } else {
                        args
                    };
                    self.sessions[target].messages.push(ChatMessage::plain(
                        "system",
                        format!("[tool:{}] {}", tool, short_args),
                    ));
                }
                AgentEvent::ToolOutput { tool, output, is_error } => {
                    let short = if output.len() > 500 {
                        format!("{}…", &output[..500])
                    } else {
                        output
                    };
                    let line = if is_error {
                        format!("[tool:{}] error: {}", tool, short)
                    } else {
                        format!("[tool:{}] {}", tool, short)
                    };
                    self.sessions[target].messages.push(ChatMessage::plain("system", line));
                }
                AgentEvent::TurnFinished {
                    usage,
                    total_calls,
                    duration_ms,
                    history,
                    total_usage,
                    last_usage,
                } => {
                    let s = &mut self.sessions[target];
                    s.agent.history = history;
                    s.agent.last_usage = last_usage;
                    s.agent.total_calls = total_calls;
                    s.total_tokens = total_usage.total_tokens as u64;
                    s.agent.total_usage = total_usage.clone();
                    // Attach the token/duration metadata to the last assistant
                    // message so draw_messages can render the telemetry footer.
                    if let Some(msg) = s.messages.iter_mut().rev().find(|m| m.role == "assistant") {
                        msg.tokens = Some(usage);
                        msg.duration_ms = Some(duration_ms);
                    }
                    if self.active_session == target {
                        self.is_thinking = false;
                        self.status_msg.clear();
                    }
                    finished = true;
                }
                AgentEvent::Error(e) => {
                    let s = &mut self.sessions[target];
                    s.messages.push(ChatMessage::plain(
                        "system",
                        format!("Error: {}", e),
                    ));
                    if self.active_session == target {
                        self.is_thinking = false;
                        self.status_msg = "Turn failed".into();
                    }
                    finished = true;
                }
                AgentEvent::Cancelled => {
                    let s = &mut self.sessions[target];
                    s.messages.push(ChatMessage::plain(
                        "system",
                        "[Interrupted by user (Ctrl+C)]".to_string(),
                    ));
                    if self.active_session == target {
                        self.is_thinking = false;
                        self.status_msg = "Cancelled".into();
                    }
                    finished = true;
                }
            }
        }

        // Restore the receiver unless this turn has ended (all channels dropped).
        if !finished {
            self.agent_events_rx = Some(rx);
        } else {
            // Turn completed: clear all agent runtime state.
            self.steering_tx = None;
            self.cancel_token = None;
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

    fn provider_name(&self) -> &'static str {
        match &self.session().agent.provider {
            Provider::OpenAI { .. } => "OpenAI",
            Provider::Anthropic { .. } => "Anthropic",
            Provider::Ollama { .. } => "Ollama",
            Provider::Router9 { .. } => "9Router",
            Provider::AntigravityZen { .. } => "Antigravity Zen",
        }
    }

    fn model_name(&self) -> String {
        match &self.session().agent.provider {
            Provider::OpenAI { model } | Provider::Anthropic { model }
            | Provider::Ollama { model } | Provider::Router9 { model }
            | Provider::AntigravityZen { model } => model.clone(),
        }
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
                self.config.apply_to_env();
                for s in &mut self.sessions {
                    s.agent.provider = ReActAgent::detect_provider();
                    s.agent.user_name = self.config.user_name.clone();
                    s.agent.persona = self.config.system_persona.clone();
                    s.agent.thinking_mode = self.config.thinking_mode.clone();
                }
                self.status_msg = "Saved ~/.cacode/config.toml ✓".into();
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
                match fs::write(&backup_path, toml::to_string_pretty(&self.config).unwrap_or_default()) {
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
                            if let Ok(cfg) = toml::from_str::<CaCodeConfig>(&content) {
                                self.config = cfg;
                                let _ = self.config.save();
                                self.config.apply_to_env();
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
        let n = self.sessions.len() + 1;
        self.sessions.push(ChatSession::new(format!("Session {}", n), &self.config));
        self.active_session = self.sessions.len() - 1;
        self.status_msg = format!("New session: {}", self.session().name);
    }

    fn close_session(&mut self) {
        if self.sessions.len() <= 1 {
            self.status_msg = "Cannot close last session".into();
            return;
        }
        let name = self.sessions[self.active_session].name.clone();
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

    fn router9_endpoint() -> String {
        let base_url = env::var("ROUTER9_URL").unwrap_or_else(|_| "http://localhost:20128".into());
        let base = base_url.trim_end_matches('/');
        if base.ends_with("/v1") || base.ends_with("/api") {
            format!("{}/chat/completions", base)
        } else {
            format!("{}/v1/chat/completions", base)
        }
    }

    fn openai_endpoint() -> String {
        let base_url = env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com".into());
        let base = base_url.trim_end_matches('/');
        if base.ends_with("/v1") {
            format!("{}/chat/completions", base)
        } else {
            format!("{}/v1/chat/completions", base)
        }
    }

    async fn test_connection(&mut self) {
        let model = self.model_name();
        let provider = self.provider_name();

        let result: Result<(), String> = match self.provider_name() {
            "9Router" => {
                let endpoint = Self::router9_endpoint();
                let api_key = env::var("ROUTER9_API_KEY").unwrap_or_else(|_| "dummy".into());
                let body = serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1
                });
                match reqwest::Client::new().post(&endpoint)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            let text = resp.text().await.unwrap_or_default();
                            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                            if let Some(err) = parsed.get("error") {
                                let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
                                Err(format!("9router error: {}", msg))
                            } else {
                                Ok(())
                            }
                        } else {
                            let text = resp.text().await.unwrap_or_default();
                            Err(format!("9router HTTP {} — {}", status, text.chars().take(200).collect::<String>()))
                        }
                    }
                    Err(e) => Err(format!("Cannot reach {} : {}", endpoint, e)),
                }
            }
            "OpenAI" => {
                let endpoint = Self::openai_endpoint();
                let api_key = env::var("OPENAI_API_KEY").unwrap_or_default();
                let body = serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1
                });
                match reqwest::Client::new().post(&endpoint)
                    .bearer_auth(&api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() { Ok(()) }
                        else {
                            let text = resp.text().await.unwrap_or_default();
                            Err(format!("OpenAI HTTP {} — {}", status, text.chars().take(200).collect::<String>()))
                        }
                    }
                    Err(e) => Err(format!("Cannot reach OpenAI: {} (API key set: {})", e, if api_key.is_empty() { "NO" } else { "yes" })),
                }
            }
            "Anthropic" => {
                let api_key = env::var("ANTHROPIC_API_KEY").unwrap_or_default();
                if api_key.is_empty() {
                    Err("ANTHROPIC_API_KEY not set".into())
                } else {
                    let body = serde_json::json!({
                    "model": model,
                    "max_tokens": 1,
                    "messages": [{"role": "user", "content": "ping"}]
                });
                match reqwest::Client::new()
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() { Ok(()) }
                        else {
                            let text = resp.text().await.unwrap_or_default();
                            Err(format!("Anthropic HTTP {} — {}", status, text.chars().take(200).collect::<String>()))
                        }
                    }
                    Err(e) => Err(format!("Cannot reach Anthropic: {}", e)),
                }
                }
            }
            "Ollama" => {
                let base_url = env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
                let endpoint = format!("{}/api/tags", base_url.trim_end_matches('/'));
                match reqwest::Client::new().get(&endpoint).send().await {
                    Ok(resp) if resp.status().is_success() => Ok(()),
                    Ok(resp) => Err(format!("Ollama HTTP {}", resp.status())),
                    Err(e) => Err(format!("Cannot reach Ollama at {}: {} (is Ollama running?)", base_url, e)),
                }
            }
            "Antigravity Zen" | "AntigravityZen" | "Antigravity" | "Zen" => {
                let endpoint = "https://antigravity.ai/zen/v1/chat/completions";
                let api_key = env::var("ANTIGRAVITY_API_KEY")
                    .or_else(|_| env::var("ANTIGRAVITY_ZEN_API_KEY"))
                    .unwrap_or_else(|_| "dummy".into());
                let body = serde_json::json!({
                    "model": model,
                    "messages": [{"role": "user", "content": "ping"}],
                    "max_tokens": 1
                });
                match reqwest::Client::new().post(endpoint)
                    .bearer_auth(api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            Ok(())
                        } else {
                            let text = resp.text().await.unwrap_or_default();
                            Err(format!("Antigravity Zen HTTP {} — {}", status, text.chars().take(200).collect::<String>()))
                        }
                    }
                    Err(e) => Err(format!("Cannot reach Antigravity Zen: {}", e)),
                }
            }
            _ => Ok(()),
        };

        match result {
            Ok(()) => self.status_msg = format!("✓ {} ({}): connection OK", provider, model),
            Err(e) => self.status_msg = format!("✗ {} error: {}", provider, e),
        }
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

pub async fn run_tui() -> Result<()> {
    let config = CaCodeConfig::load().unwrap_or_default();
    config.apply_to_env();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(config);
    let result = event_loop(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
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
            if let Some(tok) = app.cancel_token.clone() {
                tok.cancel();
            }
            return Ok(());
        }
        if app.is_thinking {
            app.spinner_frame += 1;
        }
        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
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
    // Esc in any secondary page returns straight to Chat
    if app.active_page != 0 && key.code == KeyCode::Esc && !app.settings.editing {
        app.active_page = 0;
        return true;
    }

    if app.active_page == 4 && app.settings.editing {
        return false;
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('1')) | (_, KeyCode::F(1)) => { app.active_page = 0; true }
        (KeyModifiers::CONTROL, KeyCode::Char('2')) | (_, KeyCode::F(2)) => { app.active_page = 1; app.refresh_usage_data(); true }
        (KeyModifiers::CONTROL, KeyCode::Char('3')) | (_, KeyCode::F(3)) => { app.active_page = 2; app.refresh_usage_data(); true }
        (KeyModifiers::CONTROL, KeyCode::Char('4')) | (_, KeyCode::F(4)) => { app.active_page = 3; app.refresh_skills(); true }
        (KeyModifiers::CONTROL, KeyCode::Char('5')) | (_, KeyCode::F(5)) => { app.active_page = 4; true }
        (KeyModifiers::SHIFT, KeyCode::Tab) | (_, KeyCode::F(6)) => {
            app.active_page = (app.active_page + 4) % 5;
            if app.active_page == 1 || app.active_page == 2 { app.refresh_usage_data(); }
            if app.active_page == 3 { app.refresh_skills(); }
            true
        }
        (KeyModifiers::CONTROL, KeyCode::Tab) | (_, KeyCode::F(7)) => {
            app.active_page = (app.active_page + 1) % 5;
            if app.active_page == 1 || app.active_page == 2 { app.refresh_usage_data(); }
            if app.active_page == 3 { app.refresh_skills(); }
            true
        }
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
        // Not in slash autocomplete: Tab switches agent (session).
        if key.code == KeyCode::Tab {
            app.next_session();
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
        (_, KeyCode::Tab) => {
            app.next_session();
            return Ok(());
        }
        (_, KeyCode::BackTab) => {
            app.prev_session();
            return Ok(());
        }
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if app.is_thinking {
                app.cancel_turn();
                app.is_thinking = false;
                app.status_msg = "Cancelled".into();
                let idx = app.agent_turn_session.min(app.sessions.len() - 1);
                app.sessions[idx].messages.push(ChatMessage::plain(
                    "system",
                    "[Interrupted by user (Ctrl+C)]".to_string(),
                ));
                app.agent_events_rx = None;
                app.steering_tx = None;
                app.cancel_token = None;
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => {
            app.should_quit = true;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('s')) => {
            app.active_page = 4;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('t')) | (_, KeyCode::F(10)) => {
            app.new_session();
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
            app.close_session();
        }
        (KeyModifiers::CONTROL, KeyCode::Right) | (_, KeyCode::F(9)) => {
            app.next_session();
        }
        (KeyModifiers::CONTROL, KeyCode::Left) | (_, KeyCode::F(8)) => {
            app.prev_session();
        }
        (_, KeyCode::Enter) => {
            let input = app.session().input.trim().to_string();
            if input.is_empty() { return Ok(()); }
            app.session_mut().input.clear();
            app.session_mut().cursor_pos = 0;
            app.session_mut().show_welcome = false;
            app.autocomplete_idx = 0;

            // Mid-turn steering: while a ReAct turn is in flight, a plain
            // message is captured as a `[Steering / Mid-turn]` instruction
            // that the background agent folds into its next reasoning step.
            if app.is_thinking && !input.starts_with('/') {
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
                        if arg.is_empty() {
                            app.session_mut().messages.push(ChatMessage::plain("system", format!("Current provider: {}. Usage: /provider antigravity|zen|ollama|9router|openai|anthropic\nType /providers to probe local & free provider status.", current_provider)));
                        } else {
                            let new_p = match arg.to_lowercase().as_str() {
                                "openai" => Provider::OpenAI { model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into()) },
                                "anthropic" => Provider::Anthropic { model: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-20240620".into()) },
                                "ollama" => {
                                    let model = app.config.ollama_model.clone()
                                        .or_else(|| std::env::var("OLLAMA_MODEL").ok())
                                        .unwrap_or_else(|| "llama3".into());
                                    Provider::Ollama { model }
                                }
                                "9router" | "router9" => Provider::Router9 { model: std::env::var("ROUTER9_MODEL").unwrap_or_else(|_| "ZA126_PRO".into()) },
                                "antigravity" | "antigravityzen" | "zen" => {
                                    let model = app.config.effective_antigravity_model();
                                    Provider::AntigravityZen { model }
                                }
                                other => {
                                    app.session_mut().messages.push(ChatMessage::plain("system", format!("Unknown provider: {}. Available: antigravity (or zen), ollama, 9router, openai, anthropic", other)));
                                    return Ok(());
                                }
                            };
                            let (name, m) = match &new_p {
                                Provider::AntigravityZen { model } => ("Antigravity Zen", model.clone()),
                                Provider::Ollama { model } => ("Ollama", model.clone()),
                                Provider::OpenAI { model } => ("OpenAI", model.clone()),
                                Provider::Anthropic { model } => ("Anthropic", model.clone()),
                                Provider::Router9 { model } => ("9Router", model.clone()),
                            };
                            app.session_mut().agent.provider = new_p;
                            app.status_msg = format!("Provider switched to {} ({}) ✓", name, m);
                            app.session_mut().messages.push(ChatMessage::plain("system", format!("Active provider switched to {} (model: {})", name, m)));
                        }
                        return Ok(());
                    }
                    "/help" => {
                        app.session_mut().messages.push(ChatMessage::plain("system", "Available Commands:\n  /providers         - Probe & display local & free providers (Ollama, Zen)\n  /provider <name>   - Switch provider (antigravity, zen, ollama, 9router, openai, anthropic)\n  /models            - Pick model interactively\n  /mode              - Set thinking level (Fast, Analytical, Deep)\n  /settings          - Open BYOK config\n  /monitor           - View token telemetry log\n  /report            - View cost report\n  /skills            - Skills library\n  /status            - Current system status\n  /sessions          - List chat sessions\n  /clear             - Clear chat\n  /export            - Export transcript to Markdown\n  /version           - Show version (v1.0.2)\n  /exit              - Quit\nShortcuts: Tab (autocomplete), Ctrl+T (new session), Ctrl+W (close), Esc (back to chat)"));
                        return Ok(());
                    }
                    "/clear" => {
                        app.session_mut().messages.clear();
                        app.session_mut().agent.history.clear();
                        app.session_mut().total_tokens = 0;
                        app.session_mut().show_welcome = true;
                        app.status_msg = "Cleared".into();
                        return Ok(());
                    }
                    "/compress" => {
                        app.is_thinking = true;
                        app.status_msg = "Compressing conversation history...".into();
                        terminal.draw(|f| ui(f, app))?;
                        let res = app.session_mut().agent.compress_history().await;
                        app.is_thinking = false;
                        match res {
                            Ok(msg) => {
                                app.session_mut().messages.push(ChatMessage::plain("system", msg));
                                app.status_msg = "Context compressed ✓".into();
                            }
                            Err(e) => {
                                app.session_mut().messages.push(ChatMessage::plain("system", format!("Compression failed: {}", e)));
                                app.status_msg = "Compression failed".into();
                            }
                        }
                        return Ok(());
                    }
                    "/context" => {
                        let last_u = &app.session().agent.last_usage;
                        let tot_u = &app.session().agent.total_usage;
                        let limit = app.session().agent.context_window_limit;
                        let pct = if limit > 0 { (tot_u.total_tokens as f64 / limit as f64) * 100.0 } else { 0.0 };
                        let info = format!(
                            "Context Telemetry:\n  Active Model: {}\n  Last Prompt Tokens: {}\n  Last Completion Tokens: {}\n  Total Tokens Used: {}\n  Context Window Budget: {} tokens (~{:.1}% used)",
                            app.model_name(), last_u.prompt_tokens, last_u.completion_tokens, tot_u.total_tokens, limit, pct
                        );
                        app.session_mut().messages.push(ChatMessage::plain("system", info));
                        return Ok(());
                    }
                    "/dev" | "/prd" | "/audit" | "/refact" | "/update" | "/fix" | "/bughunt" | "/test" | "/qa" => {
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
                    "/exit" => { app.should_quit = true; return Ok(()); }
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
                        app.session_mut().messages.push(ChatMessage::plain("system", "CACode v1.0.2 (Sovereign Rust Agent)"));
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

            // Append thinking mode hint if active
            let prompt_to_send = match app.config.thinking_mode.as_str() {
                "Analytical" => format!("[Mode: Analytical — break down problem step-by-step before answering]\n\n{}", input),
                "Deep" => format!("[Mode: Deep Reasoning — explore edge-cases, formal proof, and comprehensive verification]\n\n{}", input),
                _ => input.clone(),
            };

            app.start_turn(prompt_to_send);
        }
        (_, KeyCode::Backspace) => {
            let pos = app.session().cursor_pos;
            if pos > 0 {
                app.session_mut().cursor_pos -= 1;
                let cp = app.session().cursor_pos;
                app.session_mut().input.remove(cp);
            }
        }
        (_, KeyCode::Delete) => {
            let pos = app.session().cursor_pos;
            let len = app.session().input.len();
            if pos < len { app.session_mut().input.remove(pos); }
        }
        (_, KeyCode::Left)  => { if app.session().cursor_pos > 0 { app.session_mut().cursor_pos -= 1; } }
        (_, KeyCode::Right) => { if app.session().cursor_pos < app.session().input.len() { app.session_mut().cursor_pos += 1; } }
        (_, KeyCode::Home)  => { app.session_mut().cursor_pos = 0; }
        (_, KeyCode::End)   => { let len = app.session().input.len(); app.session_mut().cursor_pos = len; }
        (_, KeyCode::Up)    => { let v = app.session().scroll_offset.saturating_add(1); app.session_mut().scroll_offset = v; }
        (_, KeyCode::Down)  => { let v = app.session().scroll_offset.saturating_sub(1); app.session_mut().scroll_offset = v; }
        (_, KeyCode::Char(c)) => {
            let pos = app.session().cursor_pos;
            app.session_mut().input.insert(pos, c);
            app.session_mut().cursor_pos += 1;
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
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => { app.should_quit = true; }
        _ => {}
    }
}

fn ui(f: &mut Frame, app: &App) {
    let size = f.area();
    // Near-black background for the minimalist single-column layout.
    f.render_widget(Clear, size);
    let bg = Paragraph::new("").style(Style::default().bg(BG_DARK));
    f.render_widget(bg, size);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(1)])
        .split(size);

    let box_area = outer[0];
    let status_area = outer[1];

    // Minimalist chat page: content + centered input pill.
    // Pill is 3 rows tall (rounded border) when on the chat page, 1 row on
    // secondary pages where it doubles as the hint line.
    let input_height: u16 = if app.active_page == 0 { 3 } else { 1 };
    let gap: u16 = 1;
    let reserved = input_height + gap;
    let content_h = box_area.height.saturating_sub(reserved);

    let (cursor, pill_area) = if app.active_page == 0 {
        // Single-column minimalist layout:
        //   top pixel-block logo (only when chat is empty)
        //   chat messages / welcome
        //   centered input pill with "Ask anything..." placeholder
        let session = app.session();
        let top_logo: u16 = if session.show_welcome && session.messages.is_empty() {
            1 // logo + tagline (drawn inside content)
        } else {
            0
        };

        let content_area = Rect {
            x: box_area.x,
            y: box_area.y + top_logo,
            width: box_area.width,
            height: content_h.saturating_sub(top_logo),
        };
        draw_chat_content(f, app, content_area);

        // Centered input pill
        let pill_w = 80u16.min(box_area.width.saturating_sub(2));
        let pill = Rect {
            x: box_area.x + (box_area.width.saturating_sub(pill_w)) / 2,
            y: box_area.y + box_area.height.saturating_sub(input_height + gap),
            width: pill_w,
            height: input_height,
        };
        let cur = draw_input(f, app, pill);
        (cur, pill)
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
        let content_area = Rect {
            x: box_area.x,
            y: content_y + 2,
            width: box_area.width,
            height: content_h.saturating_sub(2),
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

    if let Some((cx, cy)) = cursor {
        f.set_cursor_position((cx, cy));
    }

    // Draw autocomplete popup if user is typing a slash command
    if app.active_page == 0 && app.modal_picker == ModalPicker::Off {
        draw_autocomplete_popup(f, app, pill_area);
    }

    // Draw modal picker if active (Models or Thinking Mode)
    if app.modal_picker != ModalPicker::Off {
        draw_modal_picker(f, app, size);
    }
}

fn draw_page_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    spans.push(Span::styled(" [Esc] Back to Chat  │  ", Style::default().fg(BRAND)));
    for (i, name) in PAGE_NAMES.iter().enumerate() {
        let style = if i == app.active_page {
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        spans.push(Span::styled(format!(" {} ", name), style));
        if i < PAGE_NAMES.len() - 1 {
            spans.push(Span::styled("┃", Style::default().fg(DIM)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_BLACK)), area);
}

#[allow(dead_code)]
fn draw_session_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    spans.push(Span::styled(" ", Style::default()));
    for (i, session) in app.sessions.iter().enumerate() {
        let style = if i == app.active_session {
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        spans.push(Span::styled(format!(" {} ", session.name), style));
        spans.push(Span::styled("┃", Style::default().fg(DIM)));
    }
    if app.sessions.len() < MAX_SESSIONS {
        spans.push(Span::styled(" + (Ctrl+T) ", Style::default().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_BLACK)), area);
}

fn draw_chat_content(f: &mut Frame, app: &App, area: Rect) {
    let session = app.session();
    if session.show_welcome && session.messages.is_empty() {
        draw_welcome(f, app, area);
    } else {
        draw_messages(f, session, area);
    }
}

fn draw_welcome(f: &mut Frame, app: &App, area: Rect) {
    let session = app.session();
    if !(session.show_welcome && session.messages.is_empty()) {
        // Non-empty conversation: compact logo + version header at the top.
        let header = Line::from(vec![
            Span::styled(
                "CACode",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " v1.0.2 · sovereign Rust agent",
                Style::default().fg(MUTED),
            ),
        ]);
        f.render_widget(
            Paragraph::new(header).style(Style::default().bg(BG_DARK)),
            Rect {
                height: 1,
                ..area
            },
        );
        return;
    }

    // Fresh conversation: centered pixel-block logo + minimalist hints.
    let mut lines: Vec<Line> = Vec::new();
    let pad = area.height.saturating_sub(8) / 2;
    for _ in 0..pad.min(2) {
        lines.push(Line::from(""));
    }

    for row in WELCOME_LINES {
        lines.push(Line::from(Span::styled(
            *row,
            Style::default().fg(GRAY),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Type a message to chat, or press / for command autocomplete",
        Style::default().fg(DIM),
    )));

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Center)
        .style(Style::default().bg(BG_DARK));
    f.render_widget(p, area);
}

fn draw_messages(f: &mut Frame, session: &ChatSession, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &session.messages {
        match msg.role.as_str() {
            "user" => {
                lines.push(Line::from(vec![
                    Span::styled("> ", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
                    Span::styled(&*msg.content, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                ]));
                lines.push(Line::from(""));
            }
            "assistant" => {
                for line in msg.content.lines() {
                    lines.push(Line::from(Span::styled(
                        line,
                        Style::default().fg(GRAY),
                    )));
                }
                // Subtle per-turn token telemetry footer: e.g. [⚡ 1,240 tokens (980 in / 260 out) • 1.2s]
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
                        Style::default().fg(DIM).add_modifier(Modifier::DIM),
                    )));
                }
                lines.push(Line::from(""));
            }
            _ => {
                for line in msg.content.lines() {
                    lines.push(Line::from(Span::styled(
                        line,
                        Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
                    )));
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
        .style(Style::default().bg(BG_BLACK));
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
                ("▸ ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD))
            } else {
                ("  ", Style::default().fg(GRAY).bg(Color::Rgb(15, 15, 20)))
            };
            let line = Line::from(vec![
                Span::styled(pfx, style),
                Span::styled(format!("{:<12}", cmd), style),
                Span::styled(format!(" {}", desc), if sel { style } else { Style::default().fg(DIM).bg(Color::Rgb(15, 15, 20)) }),
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
                    Style::default().fg(BRAND).add_modifier(Modifier::BOLD)
                } else if is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(GRAY)
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
                    Style::default().fg(BRAND).add_modifier(Modifier::BOLD)
                } else if is_current {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(GRAY)
                };
                Line::from(vec![
                    Span::styled(pfx, style),
                    Span::styled(format!("{:<14}", mode), style),
                    Span::styled(format!(" — {}", desc), if sel { style } else { Style::default().fg(DIM) }),
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
        Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
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
        Span::styled("↑↓", Style::default().fg(BRAND)),
        Span::styled(" navigate  ", Style::default().fg(DIM)),
        Span::styled("Enter", Style::default().fg(BRAND)),
        Span::styled(" select  ", Style::default().fg(DIM)),
        Span::styled("Esc", Style::default().fg(BRAND)),
        Span::styled(" cancel", Style::default().fg(DIM)),
    ]));
    f.render_widget(help, Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner });
}

fn draw_monitor_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" ◉ ", Style::default().fg(BRAND)),
        Span::styled("Token Usage Monitor", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("   (press ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(BRAND)),
        Span::styled(" to refresh, Esc to back)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(BG_BLACK)), Rect { height: 1, ..area });

    if app.usage_rows.is_empty() {
        let msg = "No usage data recorded yet. Start monitor proxy via `cacode monitor --endpoint <url>`";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(DIM))).style(Style::default().bg(BG_BLACK));
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
        .style(Style::default().fg(BRAND).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.usage_rows.iter().map(|r| {
        let ts = if r.timestamp.len() > 19 { &r.timestamp[..19] } else { &r.timestamp };
        Row::new(vec![
            ts.to_string(),
            r.model.clone(),
            format!("{}", r.total_tokens),
            format!("{:.6}", r.est_cost),
        ])
        .style(Style::default().fg(GRAY))
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
        .style(Style::default().bg(BG_BLACK));

    f.render_widget(table, table_area);
}

fn draw_report_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" 📊 ", Style::default().fg(BRAND)),
        Span::styled("Usage Report (per-model aggregation)", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("   (press ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(BRAND)),
        Span::styled(" to refresh, Esc to back)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(BG_BLACK)), Rect { height: 1, ..area });

    if app.report_rows.is_empty() {
        let msg = "No usage data to aggregate.";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(DIM))).style(Style::default().bg(BG_BLACK));
        f.render_widget(p, Rect { y: area.y + 2, height: 1, ..area });
        return;
    }

    let header = Row::new(vec!["Model", "Calls", "Prompt", "Completion", "Total", "Cost ($)"])
        .style(Style::default().fg(BRAND).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.report_rows.iter().map(|r| {
        Row::new(vec![
            r.model.clone(),
            format!("{}", r.calls),
            format!("{}", r.prompt_tokens),
            format!("{}", r.completion_tokens),
            format!("{}", r.total_tokens),
            format!("{:.6}", r.est_cost),
        ])
        .style(Style::default().fg(GRAY))
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
        .style(Style::default().fg(BRAND).add_modifier(Modifier::BOLD))
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
        .style(Style::default().bg(BG_BLACK));

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
                Span::styled(" 📈 ", Style::default().fg(BRAND)),
                Span::styled("Daily Traffic Heatmap & Top Model Analytics (last 14 days)", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
            ]);
            f.render_widget(Paragraph::new(heat_header).style(Style::default().bg(BG_BLACK)), Rect {
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
            f.render_widget(Paragraph::new(Span::styled(top_msg, Style::default().fg(Color::Yellow))).style(Style::default().bg(BG_BLACK)), Rect {
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
                    Span::styled(format!("{:<10}", row.day), Style::default().fg(GRAY)),
                    Span::styled(format!("{:>5}", row.calls), Style::default().fg(DIM)),
                    Span::styled(" calls  │  ", Style::default().fg(DIM)),
                    Span::styled(format!("{:>10}", row.tokens), Style::default().fg(Color::White)),
                    Span::styled(" tokens │  ", Style::default().fg(DIM)),
                    Span::styled(format!("${:.4}  ", row.cost), Style::default().fg(Color::Green)),
                    Span::styled(row.bar.as_str(), Style::default().fg(BRAND)),
                ]);
                f.render_widget(Paragraph::new(bar_line).style(Style::default().bg(BG_BLACK)), Rect {
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
        Span::styled(" ⚡ ", Style::default().fg(BRAND)),
        Span::styled("Skills Library (~/.cacode/skills/)", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("   (Enter load into chat, Esc back)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(BG_BLACK)), Rect { height: 1, ..area });

    if app.skills.is_empty() {
        let msg = "No skills saved yet. Use `/skill add <name>` in chat to save prompt templates.";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(DIM))).style(Style::default().bg(BG_BLACK));
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
        .style(Style::default().fg(BRAND).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app.skills.iter().enumerate().map(|(i, s)| {
        let sel = i == app.skill_selected;
        let pfx = if sel { "▸ " } else { "  " };
        let style = if sel { Style::default().fg(BRAND).add_modifier(Modifier::BOLD) } else { Style::default().fg(GRAY) };

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
        .style(Style::default().bg(BG_BLACK));

    f.render_widget(table, table_area);
}

fn draw_settings_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 4 { return; }

    let title = Line::from(vec![
        Span::styled(" ⚙  BYOK Settings & Configuration ", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled(" (Esc to back)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title).style(Style::default().bg(BG_BLACK)), Rect { height: 1, ..area });

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
            else { Style::default().fg(BRAND).add_modifier(Modifier::BOLD) }
        } else { Style::default().fg(TEXT) };

        ListItem::new(Line::from(vec![
            Span::styled(pfx, style),
            Span::styled(format!("{:20}", field.label), style),
            Span::styled(" │ ", Style::default().fg(DIM)),
            Span::styled(display, style),
        ]))
    }).collect();

    f.render_widget(List::new(items).style(Style::default().bg(BG_BLACK)), list_area);

    let help_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(2),
        width: area.width,
        height: 2,
    };
    let help = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("↑↓", Style::default().fg(BRAND)),
            Span::styled(" navigate  ", Style::default().fg(DIM)),
            Span::styled("Enter", Style::default().fg(BRAND)),
            Span::styled(" edit  ", Style::default().fg(DIM)),
            Span::styled("s", Style::default().fg(BRAND)),
            Span::styled(" save  ", Style::default().fg(DIM)),
            Span::styled("b", Style::default().fg(BRAND)),
            Span::styled(" backup  ", Style::default().fg(DIM)),
            Span::styled("r", Style::default().fg(BRAND)),
            Span::styled(" restore  ", Style::default().fg(DIM)),
            Span::styled("t", Style::default().fg(BRAND)),
            Span::styled(" test  ", Style::default().fg(DIM)),
            Span::styled("Esc", Style::default().fg(BRAND)),
            Span::styled(" back to chat", Style::default().fg(DIM)),
        ]),
        Line::from(Span::styled(
            if app.status_msg.is_empty() { "" } else { &app.status_msg },
            Style::default().fg(Color::Green),
        )),
    ]).style(Style::default().bg(BG_BLACK));
    f.render_widget(help, help_area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) -> Option<(u16, u16)> {
    if app.active_page != 0 {
        let hint = match app.active_page {
            1 => "↑↓ scroll  r refresh  Esc back to chat",
            2 => "↑↓ scroll  r refresh  Esc back to chat",
            3 => "↑↓ select  Enter load skill  Esc back to chat",
            4 => "↑↓ navigate  Enter edit  s save  Esc back to chat",
            _ => "",
        };
        let line = Line::from(Span::styled(hint, Style::default().fg(DIM)));
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(BG_BLACK)),
            area,
        );
        return None;
    }

    let session = app.session();
    let text_col = if app.is_thinking { ACCENT_DIM } else { TEXT_SOFT };

    if app.is_thinking && !session.input.is_empty() {
        // Mid-turn steering: user is actively typing a steering instruction.
        let steer_label = " ⚡ steer ";
        let line = Line::from(vec![
            Span::styled(
                steer_label,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "> ",
                Style::default().fg(ACCENT_DIM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*session.input, Style::default().fg(TEXT_SOFT)),
        ]);
        let steer_block = Block::default()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT_DIM))
            .style(Style::default().bg(BG_PILL));
        f.render_widget(steer_block, area);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(BG_PILL)),
            Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height },
        );
        let cx = area.x + 1 + steer_label.len() as u16 + 2 + session.cursor_pos as u16;
        Some((cx, area.y))
    } else if app.is_thinking {
        let (frame, emot) = THINKING_FRAMES[app.spinner_frame % THINKING_FRAMES.len()];
        let line = Line::from(vec![
            Span::styled(
                "> ",
                Style::default().fg(ACCENT_DIM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", frame),
                Style::default().fg(ACCENT_DIM).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                emot,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC),
            ),
            Span::styled(
                "  type to steer · Ctrl+C to cancel",
                Style::default().fg(MUTED),
            ),
        ]);
        let thinking_block = Block::default()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(BG_PILL));
        f.render_widget(thinking_block, area);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(BG_PILL)),
            Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height },
        );
        None
    } else {
        // Idle input pill with placeholder when empty.
        let show_placeholder = session.input.is_empty();
        let content_style = if show_placeholder {
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
        } else {
            Style::default().fg(text_col)
        };
        let line = Line::from(vec![
            Span::styled(
                "> ",
                Style::default().fg(ACCENT_DIM).add_modifier(Modifier::BOLD),
            ),
            if show_placeholder {
                Span::styled("Ask anything…", content_style)
            } else {
                Span::styled(&*session.input, content_style)
            },
        ]);
        let input_block = Block::default()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT_DIM))
            .style(Style::default().bg(BG_PILL));
        f.render_widget(input_block, area);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(BG_PILL)),
            Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height },
        );

        let cx = area.x + 2 + session.cursor_pos as u16;
        Some((cx, area.y))
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let provider = app.provider_name();
    let model = app.model_name();

    // Left side: status indicator (Build/Idle), active model name, provider name.
    let (status_dot, status_label, status_color) = if app.is_thinking {
        ("●", "Build", Color::Yellow)
    } else {
        ("●", "Idle", Color::Green)
    };

    let mut left_spans = vec![
        Span::styled(format!(" {} ", status_dot), Style::default().fg(status_color)),
        Span::styled(format!("{} ", status_label), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::styled("· ", Style::default().fg(MUTED)),
        Span::styled(format!("{} ", model), Style::default().fg(TEXT_SOFT).add_modifier(Modifier::BOLD)),
        Span::styled("· ", Style::default().fg(MUTED)),
        Span::styled(format!("{} ", provider), Style::default().fg(ACCENT_DIM)),
    ];

    if app.sessions.len() > 1 {
        left_spans.push(Span::styled("· ", Style::default().fg(MUTED)));
        left_spans.push(Span::styled(
            format!("agent {}/{} ", app.active_session + 1, app.sessions.len()),
            Style::default().fg(MUTED),
        ));
    }

    if !app.status_msg.is_empty() {
        left_spans.push(Span::styled("· ", Style::default().fg(MUTED)));
        left_spans.push(Span::styled(format!("{} ", app.status_msg), Style::default().fg(ACCENT_DIM)));
    }

    // Right side: bottom-right corner hints: 'tab switch agent' and 'ctrl+p commands'
    let right_spans = vec![
        Span::styled("tab", Style::default().fg(ACCENT_DIM)),
        Span::styled(" switch agent  ·  ", Style::default().fg(MUTED)),
        Span::styled("ctrl+p", Style::default().fg(ACCENT_DIM)),
        Span::styled(" commands ", Style::default().fg(MUTED)),
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
    let p = Paragraph::new(line).style(Style::default().bg(BG_DARK));
    f.render_widget(p, area);
}
