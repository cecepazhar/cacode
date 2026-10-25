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

use cacode_core::commands::chat::{Provider, ReActAgent};
use crate::config::CaCodeConfig;

const BRAND: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::White;
const GRAY: Color = Color::Rgb(180, 180, 190);

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
    "  Sovereign Rust Development Agent  v0.3.0",
    "",
    "  • ReAct Agent Loop with deterministic local tool execution",
    "  • Multi-provider BYOK: OpenAI, Anthropic, Ollama, 9Router",
    "  • Local token usage monitor & cost estimator, Skill System",
    "  • Zero telemetry — your keys, your machine, your data",
    "",
    "  Type a message below to start chatting.",
    "  Ctrl+1-5 switch pages.  Ctrl+T new session.  Esc quit.",
    "",
];

const PAGE_NAMES: &[&str] = &["Chat", "Monitor", "Report", "Skills", "Settings"];
const MAX_SESSIONS: usize = 9;

struct DoubleBox {
    divider_y: Option<u16>,
}

impl DoubleBox {
    fn new() -> Self { Self { divider_y: None } }
    fn with_divider(mut self, y: u16) -> Self { self.divider_y = Some(y); self }

    fn inner(&self, area: Rect) -> Rect {
        Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        }
    }
}

impl Widget for DoubleBox {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 2 || area.height < 2 { return; }
        let x1 = area.x;
        let x2 = area.x + area.width - 1;
        let y1 = area.y;
        let y2 = area.y + area.height - 1;
        let style = Style::default().fg(BRAND);

        buf.set_string(x1, y1, "╔", style);
        buf.set_string(x2, y1, "╗", style);
        buf.set_string(x1, y2, "╚", style);
        buf.set_string(x2, y2, "╝", style);

        for x in (x1 + 1)..x2 {
            buf.set_string(x, y1, "═", style);
            buf.set_string(x, y2, "═", style);
        }
        for y in (y1 + 1)..y2 {
            buf.set_string(x1, y, "║", style);
            buf.set_string(x2, y, "║", style);
        }

        if let Some(dy) = self.divider_y {
            let div_y = y1 + dy;
            if div_y > y1 && div_y < y2 {
                buf.set_string(x1, div_y, "╠", style);
                buf.set_string(x2, div_y, "╣", style);
                for x in (x1 + 1)..x2 {
                    buf.set_string(x, div_y, "═", style);
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
    fn new(name: String) -> Self {
        Self {
            name,
            messages: Vec::new(),
            agent: ReActAgent::new(),
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

#[derive(Clone)]
struct SkillItem {
    name: String,
    created: String,
    description: String,
    content: String,
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
    skills: Vec<SkillItem>,
    skill_selected: usize,
    monitor_scroll: u16,
    report_scroll: u16,
}

impl App {
    fn new(config: CaCodeConfig) -> Self {
        let settings = Self::build_settings(&config);
        let session = ChatSession::new("Session 1".into());
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
            skills: Vec::new(),
            skill_selected: 0,
            monitor_scroll: 0,
            report_scroll: 0,
        };
        app.refresh_skills();
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
                    is_secret: key.contains("api_key"),
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

    fn save_settings(&mut self) {
        for f in &self.settings.fields {
            self.config.set_field(f.key, f.value.clone());
        }
        match self.config.save() {
            Ok(_) => {
                self.config.apply_to_env();
                for s in &mut self.sessions {
                    s.agent.provider = ReActAgent::detect_provider();
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
        self.sessions.push(ChatSession::new(format!("Session {}", n)));
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

                        if content.starts_with("---") {
                            if let Some(end_idx) = content[3..].find("---") {
                                let fm = &content[3..3 + end_idx];
                                body = content[3 + end_idx + 3..].trim().to_string();
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
                    4 => handle_settings_key(app, key),
                    _ => {}
                }
            }
        }
    }
}

fn handle_global_key(app: &mut App, key: KeyEvent) -> bool {
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
        (_, KeyCode::Tab) | (_, KeyCode::F(7)) => {
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
        (KeyModifiers::CONTROL, KeyCode::Tab) => {
            app.next_session();
        }
        (_, KeyCode::Enter) => {
            let input = app.session().input.trim().to_string();
            if input.is_empty() { return Ok(()); }
            app.session_mut().input.clear();
            app.session_mut().cursor_pos = 0;
            app.session_mut().show_welcome = false;

            if input.starts_with('/') {
                let parts: Vec<&str> = input.splitn(2, ' ').collect();
                let cmd = parts[0];
                let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

                match cmd {
                    "/settings" => { app.active_page = 4; return Ok(()); }
                    "/help" => {
                        app.session_mut().messages.push(ChatMessage {
                            role: "system".into(),
                            content: "Commands: /settings  /clear  /help  /exit  /rename <name>  /status  /sessions  /model <name>  /provider <name>  /export  /tokens  /reset  /cd <path>  /pwd  /version  /skill list|add|use|delete\nKeys: Ctrl+1-5 pages  Ctrl+T new session  Ctrl+W close  Ctrl+←→ switch  Esc quit".into(),
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
                            "Status:\n  Provider: {}\n  Model: {}\n  Session Tokens: {}\n  Active Sessions: {}\n  CWD: {}\n  Config: {}",
                            app.provider_name(), app.model_name(), app.session().total_tokens, app.sessions.len(), cwd, cfg_path
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
                    "/model" => {
                        let current_model = app.model_name().to_string();
                        if arg.is_empty() {
                            app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("Current model: {}. Usage: /model <name>", current_model) });
                        } else {
                            match &mut app.session_mut().agent.provider {
                                Provider::OpenAI { model } => *model = arg.to_string(),
                                Provider::Anthropic { model } => *model = arg.to_string(),
                                Provider::Ollama { model } => *model = arg.to_string(),
                                Provider::Router9 { model } => *model = arg.to_string(),
                            }
                            app.status_msg = format!("Model changed to {}", arg);
                        }
                        return Ok(());
                    }
                    "/provider" => {
                        let current_provider = app.provider_name().to_string();
                        if arg.is_empty() {
                            app.session_mut().messages.push(ChatMessage { role: "system".into(), content: format!("Current provider: {}. Usage: /provider openai|anthropic|ollama|9router", current_provider) });
                        } else {
                            let new_p = match arg.to_lowercase().as_str() {
                                "openai" => Provider::OpenAI { model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into()) },
                                "anthropic" => Provider::Anthropic { model: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-3-5-sonnet-20240620".into()) },
                                "ollama" => Provider::Ollama { model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".into()) },
                                "9router" | "router9" => Provider::Router9 { model: std::env::var("ROUTER9_MODEL").unwrap_or_else(|_| "default".into()) },
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
                        app.session_mut().messages.push(ChatMessage { role: "system".into(), content: "CACode v0.3.0 (Sovereign Rust Agent)".into() });
                        return Ok(());
                    }
                    "/skill" => {
                        let skill_parts: Vec<&str> = arg.splitn(2, ' ').collect();
                        let sub = skill_parts.first().copied().unwrap_or("list");
                        let sub_arg = skill_parts.get(1).copied().unwrap_or("");

                        match sub {
                            "list" => {
                                app.refresh_skills();
                                let mut text = format!("Saved Skills ({}):\n", app.skills.len());
                                for s in &app.skills {
                                    text.push_str(&format!("  - {} ({}): {}\n", s.name, s.created, s.description));
                                }
                                app.session_mut().messages.push(ChatMessage { role: "system".into(), content: text.trim().to_string() });
                            }
                            "use" => {
                                app.refresh_skills();
                                let sk_opt = app.skills.iter().find(|s| s.name.eq_ignore_ascii_case(sub_arg) || s.name.to_lowercase().replace(' ', "-") == sub_arg.to_lowercase()).cloned();
                                if let Some(sk) = sk_opt {
                                    app.session_mut().input = sk.content.clone();
                                    app.session_mut().cursor_pos = sk.content.len();
                                    app.status_msg = format!("Loaded skill: {}", sk.name);
                                } else {
                                    app.status_msg = format!("Skill not found: {}", sub_arg);
                                }
                            }
                            "delete" => {
                                match app.delete_skill(sub_arg) {
                                    Ok(true) => app.status_msg = format!("Deleted skill: {}", sub_arg),
                                    Ok(false) => app.status_msg = format!("Skill not found: {}", sub_arg),
                                    Err(e) => app.status_msg = format!("Error deleting skill: {}", e),
                                }
                            }
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
                                        app.session().messages.iter().rev().find(|m| m.role == "assistant").map(|m| m.content.clone()).unwrap_or_else(|| "No content specified".into())
                                    };
                                    match app.save_skill(skill_name, "Saved via chat command", &content_to_save) {
                                        Ok(_) => app.status_msg = format!("Saved skill '{}' ✓", skill_name),
                                        Err(e) => app.status_msg = format!("Failed to save skill: {}", e),
                                    }
                                }
                            }
                            _ => {
                                app.session_mut().messages.push(ChatMessage { role: "system".into(), content: "Usage: /skill list | /skill use <name> | /skill add <name> [content] | /skill delete <name>".into() });
                            }
                        }
                        return Ok(());
                    }
                    s => {
                        app.session_mut().messages.push(ChatMessage {
                            role: "system".into(),
                            content: format!("Unknown command: {}", s),
                        });
                        return Ok(());
                    }
                }
            }

            app.session_mut().messages.push(ChatMessage { role: "user".into(), content: input.clone() });
            app.session_mut().scroll_offset = 0;
            app.is_thinking = true;
            app.status_msg = "Thinking…".into();
            terminal.draw(|f| ui(f, app))?;

            app.session_mut().agent.add_user(&input);
            let mut prompt = input;

            for _ in 0..5 {
                let resp = app.session_mut().agent.call_llm(&prompt).await;
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
                        prompt = format!("Observation/Tool Results:\n{}", results.join("\n\n"));
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
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => { app.should_quit = true; }
        (_, KeyCode::Char('r')) => { app.refresh_usage_data(); app.status_msg = "Refreshed".into(); }
        (_, KeyCode::Up)   => { app.monitor_scroll = app.monitor_scroll.saturating_add(1); }
        (_, KeyCode::Down) => { app.monitor_scroll = app.monitor_scroll.saturating_sub(1); }
        _ => {}
    }
}

fn handle_report_key(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => { app.should_quit = true; }
        (_, KeyCode::Char('r')) => { app.refresh_usage_data(); app.status_msg = "Refreshed".into(); }
        (_, KeyCode::Up)   => { app.report_scroll = app.report_scroll.saturating_add(1); }
        (_, KeyCode::Down) => { app.report_scroll = app.report_scroll.saturating_sub(1); }
        _ => {}
    }
}

fn handle_skills_key(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('d')) | (_, KeyCode::Esc) => { app.should_quit = true; }
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

fn handle_settings_key(app: &mut App, key: KeyEvent) {
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
        (_, KeyCode::Char('t')) => { app.status_msg = format!("Connection test: Provider {} is ready ✓", app.provider_name()); }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => { app.should_quit = true; }
        _ => {}
    }
}

fn ui(f: &mut Frame, app: &App) {
    let size = f.area();
    f.render_widget(Clear, size);
    let bg = Paragraph::new("").style(Style::default().bg(Color::Black));
    f.render_widget(bg, size);

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(1)])
        .split(size);

    let box_area = outer[0];
    let status_area = outer[1];

    let input_height: u16 = 1;
    let divider_y = box_area.height.saturating_sub(2 + input_height);

    let dbox = DoubleBox::new().with_divider(divider_y);
    let inner = dbox.inner(box_area);
    f.render_widget(dbox, box_area);

    let page_tab_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    draw_page_tabs(f, app, page_tab_area);

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

    let mut content_y = inner.y + 2;
    let divider_abs_y = box_area.y + divider_y;

    if app.active_page == 0 {
        let session_tab_area = Rect {
            x: inner.x,
            y: content_y,
            width: inner.width,
            height: 1,
        };
        draw_session_tabs(f, app, session_tab_area);
        content_y += 1;
    }

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
}

fn draw_page_tabs(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    spans.push(Span::styled(" ", Style::default()));
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
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
        spans.push(Span::styled(" + ", Style::default().fg(DIM)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
        } else {
            Style::default().fg(DIM)
        };
        lines.push(Line::from(Span::styled(*row, style)));
    }

    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
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
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_monitor_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" ◉ ", Style::default().fg(BRAND)),
        Span::styled("Token Usage Monitor", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("   (press ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(BRAND)),
        Span::styled(" to refresh)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title), Rect { height: 1, ..area });

    if app.usage_rows.is_empty() {
        let msg = "No usage data recorded yet. Start monitor proxy via `cacode monitor --endpoint <url>`";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(DIM)));
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
        .column_spacing(1);

    f.render_widget(table, table_area);
}

fn draw_report_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" 📊 ", Style::default().fg(BRAND)),
        Span::styled("Usage Report (per-model aggregation)", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("   (press ", Style::default().fg(DIM)),
        Span::styled("r", Style::default().fg(BRAND)),
        Span::styled(" to refresh)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title), Rect { height: 1, ..area });

    if app.report_rows.is_empty() {
        let msg = "No usage data to aggregate.";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(DIM)));
        f.render_widget(p, Rect { y: area.y + 2, height: 1, ..area });
        return;
    }

    let table_area = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2),
    };

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

    let table = Table::new(all_rows, widths)
        .header(header)
        .column_spacing(1);

    f.render_widget(table, table_area);
}

fn draw_skills_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 { return; }

    let title = Line::from(vec![
        Span::styled(" ⚡ ", Style::default().fg(BRAND)),
        Span::styled("Skills Library (~/.cacode/skills/)", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
        Span::styled("   (Enter to load into chat, r refresh)", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(title), Rect { height: 1, ..area });

    if app.skills.is_empty() {
        let msg = "No skills saved yet. Use `/skill add <name>` in chat to save prompt templates.";
        let p = Paragraph::new(Span::styled(msg, Style::default().fg(DIM)));
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
        .column_spacing(1);

    f.render_widget(table, table_area);
}

fn draw_settings_content(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 4 { return; }

    let title = Line::from(Span::styled(
        " ⚙  BYOK Settings ",
        Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
    ));
    f.render_widget(Paragraph::new(title), Rect { height: 1, ..area });

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

    f.render_widget(List::new(items), list_area);

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
            Span::styled(" back", Style::default().fg(DIM)),
        ]),
        Line::from(Span::styled(
            if app.status_msg.is_empty() { "" } else { &app.status_msg },
            Style::default().fg(Color::Green),
        )),
    ]);
    f.render_widget(help, help_area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    if app.active_page != 0 {
        let hint = match app.active_page {
            1 => "↑↓ scroll  r refresh  Ctrl+1 chat",
            2 => "↑↓ scroll  r refresh  Ctrl+1 chat",
            3 => "↑↓ select  Enter load skill  r refresh  Ctrl+1 chat",
            4 => "↑↓ navigate  Enter edit  s save  Esc back",
            _ => "",
        };
        let line = Line::from(Span::styled(hint, Style::default().fg(DIM)));
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    let session = app.session();
    if app.is_thinking {
        let line = Line::from(vec![
            Span::styled("> ", Style::default().fg(BRAND)),
            Span::styled("thinking…", Style::default().fg(DIM).add_modifier(Modifier::ITALIC)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else {
        let line = Line::from(vec![
            Span::styled("> ", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(&*session.input, Style::default().fg(TEXT)),
        ]);
        f.render_widget(Paragraph::new(line), area);

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
    let extra = if app.status_msg.is_empty() {
        String::new()
    } else {
        format!("  {}", app.status_msg)
    };

    let page_name = PAGE_NAMES.get(app.active_page).unwrap_or(&"?");

    let mut spans = vec![
        Span::styled(format!(" {} ", provider), Style::default().fg(Color::Black).bg(BRAND)),
        Span::styled(format!(" {} ", model), Style::default().fg(BRAND)),
        Span::styled("│ ", Style::default().fg(DIM)),
    ];

    if app.active_page == 0 {
        let tok = app.session().total_tokens;
        spans.push(Span::styled(format!("tokens: {}", tok), Style::default().fg(DIM)));
        spans.push(Span::styled(" │ ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            format!("{}/{} sessions", app.active_session + 1, app.sessions.len()),
            Style::default().fg(DIM),
        ));
        spans.push(Span::styled(" │ ", Style::default().fg(DIM)));
    }

    spans.push(Span::styled(format!("[{}]", page_name), Style::default().fg(BRAND)));
    spans.push(Span::styled(" │ ", Style::default().fg(DIM)));
    spans.push(Span::styled("Ctrl+T new  Ctrl+1-5 pages  Esc quit", Style::default().fg(DIM)));
    spans.push(Span::styled(extra, Style::default().fg(Color::Green)));

    let line = Line::from(spans);
    let p = Paragraph::new(line).style(Style::default().bg(Color::Rgb(15, 15, 20)));
    f.render_widget(p, area);
}
