use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, Paragraph, Row, Table, Widget, Wrap},
    Frame, Terminal,
};
use rusqlite::Connection;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::env;

use cacode_core::commands::chat::{Provider, ReActAgent};
use crate::config::CaCodeConfig;

const BRAND: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::White;
const GRAY: Color = Color::Rgb(180, 180, 190);
const BG_BLACK: Color = Color::Rgb(0, 0, 0);

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
    "  Sovereign Rust Development Agent  v1.0.1",
    "",
    "  • ReAct Agent Loop with deterministic local tool execution",
    "  • Multi-provider BYOK: OpenAI, Anthropic, Ollama, 9Router",
    "  • Local token usage monitor & cost estimator, Skill System",
    "  • Zero telemetry — your keys, your machine, your data",
    "",
    "  Type a message to chat, or '/' for command autocomplete",
    "    /models   /mode   /settings   /monitor   /report   /skills",
    "",
];

const PAGE_NAMES: &[&str] = &["Chat", "Monitor", "Report", "Skills", "Settings"];
const MAX_SESSIONS: usize = 9;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/models", "Interactive AI model picker"),
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
    ("/version", "Show CACode version (v1.0.1)"),
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

struct BorderlessLayout {
    divider_y: Option<u16>,
}

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
        }
    }

    fn model_name(&self) -> String {
        match &self.session().agent.provider {
            Provider::OpenAI { model } | Provider::Anthropic { model }
            | Provider::Ollama { model } | Provider::Router9 { model } => model.clone(),
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
            _ => Ok(()),
        };

        match result {
            Ok(()) => self.status_msg = format!("✓ {} ({}): connection OK", provider, model),
            Err(e) => self.status_msg = format!("✗ {} error: {}", provider, e),
        }
    }

    fn refresh_models_cache(&mut self) {
        // Preset high-value models available across 9router and providers
        self.models_cache = vec![
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
        terminal.draw(|f| ui(f, app))?;
        if app.should_quit { return Ok(()); }
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
                            }
                            app.config.router9_model = Some(m.clone());
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
    }

    // 3. Regular chat inputs & shortcuts
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
            if app.is_thinking {
                app.is_thinking = false;
                app.status_msg = "Cancelled".into();
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
                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: "Usage: /skill add <name> [content]".into() });
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
                                    app.session_mut().messages.push(ChatMessage {
                                        role: "system".into(),
                                        content: format!("Unknown mode '{}'. Choose: fast, analytical, deep", other),
                                    });
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
                            }
                            app.config.router9_model = Some(arg.to_string());
                            let _ = app.config.save();
                            app.status_msg = format!("Model changed to {}", arg);
                        }
                        return Ok(());
                    }
                    "/provider" => {
                        let current_provider = app.provider_name().to_string();
                        if arg.is_empty() {
                            app.session_mut().messages.push(ChatMessage {
                                role: "system".into(),
                                content: format!("Current provider: {}. Usage: /provider openai|anthropic|ollama|9router", current_provider),
                            });
                        } else {
                            let new_p = match arg.to_lowercase().as_str() {
                                "openai" => Provider::OpenAI { model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into()) },
                                "anthropic" => Provider::Anthropic { model: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-20240620".into()) },
                                "ollama" => Provider::Ollama { model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".into()) },
                                "9router" | "router9" => Provider::Router9 { model: std::env::var("ROUTER9_MODEL").unwrap_or_else(|_| "ZA126_PRO".into()) },
                                other => {
                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("Unknown provider: {}", other) });
                                    return Ok(());
                                }
                            };
                            app.session_mut().agent.provider = new_p;
                            app.status_msg = format!("Provider switched to {}", arg);
                        }
                        return Ok(());
                    }
                    "/help" => {
                        app.session_mut().messages.push(ChatMessage {
                            role: "system".into(),
                            content: "Available Commands:\n  /models            - Pick model interactively\n  /mode              - Set thinking level (Fast, Analytical, Deep)\n  /settings          - Open BYOK config\n  /monitor           - View token telemetry log\n  /report            - View cost report\n  /skills            - Skills library\n  /status            - Current system status\n  /sessions          - List chat sessions\n  /clear             - Clear chat\n  /export            - Export transcript to Markdown\n  /version           - Show version (v1.0.1)\n  /exit              - Quit\nShortcuts: Tab (autocomplete), Ctrl+T (new session), Ctrl+W (close), Esc (back to chat)".into(),
                        });
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
                                app.session_mut().messages.push(ChatMessage { role: "system".into(), content: msg });
                                app.status_msg = "Context compressed ✓".into();
                            }
                            Err(e) => {
                                app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("Compression failed: {}", e) });
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
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: info });
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
                            // Prepend skill instructions to the prompt
                            let full_prompt = format!("[Skill Directive: {}]\n\n{}\n\nTask:\n{}", skill_name.to_uppercase(), skill_body, arg);
                            app.session_mut().messages.push(ChatMessage { role: "user".into(), content: full_prompt.clone() });
                            app.session_mut().scroll_offset = 0;
                            app.is_thinking = true;
                            app.status_msg = format!("Executing /{}...", skill_name);
                            terminal.draw(|f| ui(f, app))?;

                            app.session_mut().agent.add_user(&full_prompt);
                            let mut current_prompt = full_prompt;

                            for _ in 0..5 {
                                let resp = app.session_mut().agent.call_llm(&current_prompt).await;
                                match resp {
                                    Ok(resp) => {
                                        let tools = app.session().agent.parse_tool_calls(&resp);
                                        app.session_mut().agent.add_assistant(&resp);

                                        if tools.is_empty() {
                                            app.session_mut().messages.push(ChatMessage { role: "assistant".into(), content: resp });
                                            break;
                                        }

                                        app.session_mut().messages.push(ChatMessage { role: "assistant".into(), content: resp });
                                        let mut results = Vec::new();
                                        for call in &tools {
                                            let hdr = format!("[tool:{}]", call.tool);
                                            match app.session().agent.exec_tool(call) {
                                                Ok(res) => {
                                                    let short = if res.len() > 500 { format!("{}…", &res[..500]) } else { res.clone() };
                                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("{} {}", hdr, short) });
                                                    results.push(format!("Tool '{}' output:\n{}", call.tool, res));
                                                }
                                                Err(e) => {
                                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("{} error: {}", hdr, e) });
                                                    results.push(format!("Tool '{}' error: {}", call.tool, e));
                                                }
                                            }
                                        }
                                        current_prompt = format!("Observation/Tool Results:\n{}", results.join("\n\n"));
                                        terminal.draw(|f| ui(f, app))?;
                                    }
                                    Err(e) => {
                                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("Error: {}", e) });
                                        break;
                                    }
                                }
                            }
                            app.is_thinking = false;
                            app.status_msg.clear();
                            return Ok(());
                        }
                    }
                    "/exit" => { app.should_quit = true; return Ok(()); }
                    "/rename" => {
                        if arg.is_empty() {
                            app.session_mut().messages.push(ChatMessage { role: "system".into(), content: "Usage: /rename <name>".into() });
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
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: info });
                        return Ok(());
                    }
                    "/sessions" => {
                        let mut list = String::from("Sessions:\n");
                        for (i, s) in app.sessions.iter().enumerate() {
                            let active = if i == app.active_session { " (active)" } else { "" };
                            list.push_str(&format!("  {}. {}{} [{} messages]\n", i + 1, s.name, active, s.messages.len()));
                        }
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: list.trim().to_string() });
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
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: info });
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
                            app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("CWD: {}", cwd) });
                        } else {
                            match std::env::set_current_dir(arg) {
                                Ok(_) => {
                                    let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                                    app.status_msg = format!("Changed CWD to {}", cwd);
                                }
                                Err(e) => {
                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("cd error: {}", e) });
                                }
                            }
                        }
                        return Ok(());
                    }
                    "/pwd" => {
                        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into());
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: cwd });
                        return Ok(());
                    }
                    "/version" => {
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: "CACode v1.0.1 (Sovereign Rust Agent)".into() });
                        return Ok(());
                    }
                    s => {
                        app.session_mut().messages.push(ChatMessage {
                            role: "system".into(),
                            content: format!("Unknown command: {}. Type /help or / for suggestions.", s),
                        });
                        return Ok(());
                    }
                }
            }

            // Chat dispatch
            app.session_mut().messages.push(ChatMessage { role: "user".into(), content: input.clone() });
            app.session_mut().scroll_offset = 0;
            app.is_thinking = true;
            app.status_msg = "Thinking…".into();
            terminal.draw(|f| ui(f, app))?;

            // Append thinking mode hint if active
            let prompt_to_send = match app.config.thinking_mode.as_str() {
                "Analytical" => format!("[Mode: Analytical — break down problem step-by-step before answering]\n\n{}", input),
                "Deep" => format!("[Mode: Deep Reasoning — explore edge-cases, formal proof, and comprehensive verification]\n\n{}", input),
                _ => input.clone(),
            };

            app.session_mut().agent.add_user(&prompt_to_send);
            let mut current_prompt = prompt_to_send;

            for _ in 0..5 {
                let resp = app.session_mut().agent.call_llm(&current_prompt).await;
                match resp {
                    Ok(resp) => {
                        let tools = app.session().agent.parse_tool_calls(&resp);
                        app.session_mut().agent.add_assistant(&resp);

                        if tools.is_empty() {
                            app.session_mut().messages.push(ChatMessage { role: "assistant".into(), content: resp });
                            break;
                        }

                        app.session_mut().messages.push(ChatMessage { role: "assistant".into(), content: resp });
                        let mut results = Vec::new();
                        for call in &tools {
                            let hdr = format!("[tool:{}]", call.tool);
                            match app.session().agent.exec_tool(call) {
                                Ok(res) => {
                                    let short = if res.len() > 500 { format!("{}…", &res[..500]) } else { res.clone() };
                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("{} {}", hdr, short) });
                                    results.push(format!("Tool '{}' output:\n{}", call.tool, res));
                                }
                                Err(e) => {
                                    app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("{} error: {}", hdr, e) });
                                    results.push(format!("Tool '{}' error: {}", call.tool, e));
                                }
                            }
                        }
                        current_prompt = format!("Observation/Tool Results:\n{}", results.join("\n\n"));
                        terminal.draw(|f| ui(f, app))?;
                    }
                    Err(e) => {
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("Error: {}", e) });
                        break;
                    }
                }
            }
            app.is_thinking = false;
            app.status_msg.clear();
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
    // 100% full black background
    f.render_widget(Clear, size);
    let bg = Paragraph::new("").style(Style::default().bg(BG_BLACK));
    f.render_widget(bg, size);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(1)])
        .split(size);

    let box_area = outer[0];
    let status_area = outer[1];

    let input_height: u16 = 1;
    let divider_y = box_area.height.saturating_sub(2 + input_height);

    let blayout = BorderlessLayout::new().with_divider(divider_y);
    let inner = blayout.inner(box_area);
    f.render_widget(blayout, box_area);

    let mut content_y = inner.y;

    // Only show page tabs if NOT on chat page (chat page is 100% clean borderless)
    if app.active_page != 0 {
        let page_tab_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        draw_page_tabs(f, app, page_tab_area);
        content_y = inner.y + 2;
        let sep_area = Rect {
            x: inner.x,
            y: inner.y + 1,
            width: inner.width,
            height: 1,
        };
        let sep_str: String = "─".repeat(inner.width as usize);
        f.render_widget(
            Paragraph::new(Span::styled(sep_str, Style::default().fg(DIM))),
            sep_area,
        );
    } else if app.sessions.len() > 1 {
        // In chat, only show session tabs if there are 2 or more sessions
        let session_tab_area = Rect {
            x: inner.x,
            y: content_y,
            width: inner.width,
            height: 1,
        };
        draw_session_tabs(f, app, session_tab_area);
        content_y += 1;
    }

    let divider_abs_y = box_area.y + divider_y;
    let content_area = Rect {
        x: inner.x,
        y: content_y,
        width: inner.width,
        height: divider_abs_y.saturating_sub(content_y),
    };

    let input_area = Rect {
        x: inner.x,
        y: divider_abs_y + 1,
        width: inner.width,
        height: input_height,
    };

    match app.active_page {
        0 => draw_chat_content(f, app, content_area),
        1 => draw_monitor_content(f, app, content_area),
        2 => draw_report_content(f, app, content_area),
        3 => draw_skills_content(f, app, content_area),
        4 => draw_settings_content(f, app, content_area),
        _ => {}
    }

    draw_input(f, app, input_area);
    draw_status(f, app, status_area);

    // Draw autocomplete popup if user is typing a slash command
    if app.active_page == 0 && app.modal_picker == ModalPicker::Off {
        draw_autocomplete_popup(f, app, input_area);
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
        draw_welcome(f, area);
    } else {
        draw_messages(f, session, area);
    }
}

fn draw_welcome(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    let logo_height = LOGO.len() as u16 + WELCOME_LINES.len() as u16;
    let pad = area.height.saturating_sub(logo_height) / 2;
    for _ in 0..pad.min(4) {
        lines.push(Line::from(""));
    }

    for row in LOGO {
        lines.push(Line::from(Span::styled(
            *row,
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
        )));
    }

    for row in WELCOME_LINES {
        let style = if row.starts_with("  •") {
            Style::default().fg(GRAY)
        } else if row.contains("Sovereign") {
            Style::default().fg(BRAND)
        } else if row.contains('/') {
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        lines.push(Line::from(Span::styled(*row, style)));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false }).style(Style::default().bg(BG_BLACK));
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

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    if app.active_page != 0 {
        let hint = match app.active_page {
            1 => "↑↓ scroll  r refresh  Esc back to chat",
            2 => "↑↓ scroll  r refresh  Esc back to chat",
            3 => "↑↓ select  Enter load skill  Esc back to chat",
            4 => "↑↓ navigate  Enter edit  s save  Esc back to chat",
            _ => "",
        };
        let line = Line::from(Span::styled(hint, Style::default().fg(DIM)));
        f.render_widget(Paragraph::new(line).style(Style::default().bg(BG_BLACK)), area);
        return;
    }

    let session = app.session();
    if app.is_thinking {
        let (frame, emot) = THINKING_FRAMES[app.spinner_frame % THINKING_FRAMES.len()];
        let line = Line::from(vec![
            Span::styled("> ", Style::default().fg(BRAND)),
            Span::styled(format!("{} ", frame), Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(emot, Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC)),
            Span::styled("  …", Style::default().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line).style(Style::default().bg(BG_BLACK)), area);
    } else {
        let line = Line::from(vec![
            Span::styled("> ", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(&*session.input, Style::default().fg(TEXT)),
        ]);
        f.render_widget(Paragraph::new(line).style(Style::default().bg(BG_BLACK)), area);

        let cx = area.x + 2 + session.cursor_pos as u16;
        let cy = area.y;
        if cx < area.x + area.width {
            f.set_cursor_position((cx, cy));
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let provider = app.provider_name();
    let model = app.model_name();
    let thinking = app.thinking_mode();
    let extra = if app.status_msg.is_empty() {
        String::new()
    } else {
        format!("  {}", app.status_msg)
    };

    let mut spans = vec![
        Span::styled(" CACode v1.0.1 ", Style::default().fg(Color::Black).bg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {} ", provider), Style::default().fg(BRAND)),
        Span::styled("│ ", Style::default().fg(DIM)),
        Span::styled(format!("{} ", model), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(DIM)),
        Span::styled(format!("[{}] ", thinking), Style::default().fg(Color::Yellow)),
        Span::styled("│ ", Style::default().fg(DIM)),
    ];

    if app.active_page == 0 {
        let tot = &app.session().agent.total_usage;
        let limit = app.session().agent.context_window_limit;
        let current_ctx = tot.total_tokens;
        let pct = if limit > 0 {
            (current_ctx as f64 / limit as f64) * 100.0
        } else {
            0.0
        };
        let ctx_style = if pct > 80.0 {
            Style::default().fg(Color::Red)
        } else if pct > 50.0 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(DIM)
        };

        spans.push(Span::styled(format!("ctx: {}k/{}k ({:.1}%) ", current_ctx / 1000, limit / 1000, pct), ctx_style));
        spans.push(Span::styled("│ ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            format!("🔗 {} calls  ", tot.total_tokens / 256),
            Style::default().fg(DIM),
        ));
        spans.push(Span::styled("│ ", Style::default().fg(DIM)));
        if app.sessions.len() > 1 {
            spans.push(Span::styled(
                format!("{}/{} sessions │ ", app.active_session + 1, app.sessions.len()),
                Style::default().fg(DIM),
            ));
        }
    } else {
        let page_name = PAGE_NAMES.get(app.active_page).unwrap_or(&"?");
        spans.push(Span::styled(format!("[{}] │ ", page_name), Style::default().fg(BRAND)));
    }

    spans.push(Span::styled("Type / for commands  Esc quit", Style::default().fg(DIM)));
    spans.push(Span::styled(extra, Style::default().fg(Color::Green)));

    let line = Line::from(spans);
    let p = Paragraph::new(line).style(Style::default().bg(BG_BLACK));
    f.render_widget(p, area);
}
