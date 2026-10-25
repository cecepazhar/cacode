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
    widgets::{Clear, List, ListItem, Paragraph, Widget, Wrap},
    Frame, Terminal,
};
use std::io;

use cacode_core::commands::chat::{Provider, ReActAgent};
use crate::config::CaCodeConfig;

const BRAND: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const TEXT: Color = Color::White;
const GRAY: Color = Color::Rgb(180, 180, 190);

/// ASCII art figlet-style title
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
    "  Sovereign Rust Development Agent  v0.2.0",
    "",
    "  • ReAct Agent Loop with deterministic local tool execution",
    "  • Multi-provider BYOK: OpenAI, Anthropic, Ollama, 9Router",
    "  • Local token usage monitor & cost estimator",
    "  • Zero telemetry — your keys, your machine, your data",
    "",
    "  Type a message below to start chatting.",
    "  /settings or Ctrl+S to configure API keys.",
    "  /help for all commands.  Esc or Ctrl+D to quit.",
    "",
];

// ── Double-border box widget ──────────────────────────────────────────
// Ratatui's Block doesn't support double-line borders natively in all versions,
// so we draw them manually for the outer chrome.

struct DoubleBox {
    /// If true, draw a horizontal divider at `divider_y` (relative to top).
    divider_y: Option<u16>,
}

impl DoubleBox {
    fn new() -> Self { Self { divider_y: None } }
    fn with_divider(mut self, y: u16) -> Self { self.divider_y = Some(y); self }

    /// Returns the inner rect (content area excluding borders).
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

        // Corners
        buf.set_string(x1, y1, "╔", style);
        buf.set_string(x2, y1, "╗", style);
        buf.set_string(x1, y2, "╚", style);
        buf.set_string(x2, y2, "╝", style);

        // Top & bottom edges
        for x in (x1 + 1)..x2 {
            buf.set_string(x, y1, "═", style);
            buf.set_string(x, y2, "═", style);
        }
        // Left & right edges
        for y in (y1 + 1)..y2 {
            buf.set_string(x1, y, "║", style);
            buf.set_string(x2, y, "║", style);
        }

        // Optional horizontal divider ╠═══╣
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

// ── Data types ────────────────────────────────────────────────────────

#[derive(Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Clone, PartialEq)]
enum Page { Chat, Settings }

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

struct App {
    page: Page,
    messages: Vec<ChatMessage>,
    input: String,
    cursor_pos: usize,
    scroll_offset: u16,
    agent: ReActAgent,
    config: CaCodeConfig,
    settings: SettingsState,
    status_msg: String,
    is_thinking: bool,
    total_tokens: u64,
    should_quit: bool,
    show_welcome: bool,
}

impl App {
    fn new(config: CaCodeConfig) -> Self {
        let agent = ReActAgent::new();
        let settings = Self::build_settings(&config);
        Self {
            page: Page::Chat,
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll_offset: 0,
            agent,
            config,
            settings,
            status_msg: String::new(),
            is_thinking: false,
            total_tokens: 0,
            should_quit: false,
            show_welcome: true,
        }
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
        match &self.agent.provider {
            Provider::OpenAI { .. } => "OpenAI",
            Provider::Anthropic { .. } => "Anthropic",
            Provider::Ollama { .. } => "Ollama",
            Provider::Router9 { .. } => "9Router",
        }
    }

    fn model_name(&self) -> &str {
        match &self.agent.provider {
            Provider::OpenAI { model } | Provider::Anthropic { model }
            | Provider::Ollama { model } | Provider::Router9 { model } => model,
        }
    }

    fn save_settings(&mut self) {
        for f in &self.settings.fields {
            self.config.set_field(f.key, f.value.clone());
        }
        match self.config.save() {
            Ok(_) => {
                self.config.apply_to_env();
                self.agent.provider = ReActAgent::detect_provider();
                self.status_msg = "Saved ~/.cacode/config.toml ✓".into();
            }
            Err(e) => self.status_msg = format!("Save failed: {}", e),
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────

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
                match app.page {
                    Page::Chat => handle_chat_key(app, key, terminal).await?,
                    Page::Settings => handle_settings_key(app, key),
                }
            }
        }
    }
}

// ── Key handlers ──────────────────────────────────────────────────────

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
            app.page = Page::Settings;
        }
        (_, KeyCode::Enter) => {
            let input = app.input.trim().to_string();
            if input.is_empty() { return Ok(()); }
            app.input.clear();
            app.cursor_pos = 0;
            app.show_welcome = false;

            // Slash commands
            match input.as_str() {
                "/settings" => { app.page = Page::Settings; return Ok(()); }
                "/help" => {
                    app.messages.push(ChatMessage {
                        role: "system".into(),
                        content: "Commands: /settings  /clear  /help  /exit\nKeys:     Ctrl+S settings  Ctrl+D/Esc quit  ↑↓ scroll".into(),
                    });
                    return Ok(());
                }
                "/clear" => {
                    app.messages.clear();
                    app.agent.history.clear();
                    app.total_tokens = 0;
                    app.show_welcome = true;
                    app.status_msg = "Cleared".into();
                    return Ok(());
                }
                "/exit" => { app.should_quit = true; return Ok(()); }
                s if s.starts_with('/') => {
                    app.messages.push(ChatMessage {
                        role: "system".into(),
                        content: format!("Unknown command: {}", s),
                    });
                    return Ok(());
                }
                _ => {}
            }

            // Chat message
            app.messages.push(ChatMessage { role: "user".into(), content: input.clone() });
            app.scroll_offset = 0;
            app.is_thinking = true;
            app.status_msg = "Thinking…".into();
            terminal.draw(|f| ui(f, app))?;

            app.agent.add_user(&input);
            let mut prompt = input;

            for _ in 0..5 {
                match app.agent.call_llm(&prompt).await {
                    Ok(resp) => {
                        let tools = app.agent.parse_tool_calls(&resp);
                        app.agent.add_assistant(&resp);

                        if tools.is_empty() {
                            app.messages.push(ChatMessage { role: "assistant".into(), content: resp });
                            break;
                        }

                        app.messages.push(ChatMessage { role: "assistant".into(), content: resp });
                        let mut results = Vec::new();
                        for call in &tools {
                            let hdr = format!("[tool:{}]", call.tool);
                            match app.agent.exec_tool(call) {
                                Ok(res) => {
                                    let short = if res.len() > 500 { format!("{}…", &res[..500]) } else { res.clone() };
                                    app.messages.push(ChatMessage { role: "system".into(), content: format!("{} {}", hdr, short) });
                                    results.push(format!("Tool '{}' output:\n{}", call.tool, res));
                                }
                                Err(e) => {
                                    app.messages.push(ChatMessage { role: "system".into(), content: format!("{} error: {}", hdr, e) });
                                    results.push(format!("Tool '{}' error: {}", call.tool, e));
                                }
                            }
                        }
                        prompt = format!("Observation/Tool Results:\n{}", results.join("\n\n"));
                        terminal.draw(|f| ui(f, app))?;
                    }
                    Err(e) => {
                        app.messages.push(ChatMessage { role: "system".into(), content: format!("Error: {}", e) });
                        break;
                    }
                }
            }
            app.is_thinking = false;
            app.status_msg.clear();
        }
        (_, KeyCode::Backspace) => {
            if app.cursor_pos > 0 { app.cursor_pos -= 1; app.input.remove(app.cursor_pos); }
        }
        (_, KeyCode::Delete) => {
            if app.cursor_pos < app.input.len() { app.input.remove(app.cursor_pos); }
        }
        (_, KeyCode::Left)  => { if app.cursor_pos > 0 { app.cursor_pos -= 1; } }
        (_, KeyCode::Right) => { if app.cursor_pos < app.input.len() { app.cursor_pos += 1; } }
        (_, KeyCode::Home)  => { app.cursor_pos = 0; }
        (_, KeyCode::End)   => { app.cursor_pos = app.input.len(); }
        (_, KeyCode::Up)    => { app.scroll_offset = app.scroll_offset.saturating_add(1); }
        (_, KeyCode::Down)  => { app.scroll_offset = app.scroll_offset.saturating_sub(1); }
        (_, KeyCode::Char(c)) => {
            app.input.insert(app.cursor_pos, c);
            app.cursor_pos += 1;
            app.show_welcome = false;
        }
        _ => {}
    }
    Ok(())
}

fn handle_settings_key(app: &mut App, key: KeyEvent) {
    if app.settings.editing {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => { app.settings.editing = false; }
            KeyCode::Backspace => { app.settings.fields[app.settings.selected].value.pop(); }
            KeyCode::Char(c) => { app.settings.fields[app.settings.selected].value.push(c); }
            _ => {}
        }
        return;
    }
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc) | (KeyModifiers::CONTROL, KeyCode::Char('s')) => { app.page = Page::Chat; }
        (_, KeyCode::Up) | (_, KeyCode::Char('k')) => {
            if app.settings.selected > 0 { app.settings.selected -= 1; }
        }
        (_, KeyCode::Down) | (_, KeyCode::Char('j')) => {
            if app.settings.selected < app.settings.fields.len() - 1 { app.settings.selected += 1; }
        }
        (_, KeyCode::Enter) | (_, KeyCode::Char('e')) => { app.settings.editing = true; }
        (_, KeyCode::Char('s')) => { app.save_settings(); app.page = Page::Chat; }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => { app.should_quit = true; }
        _ => {}
    }
}

// ── Drawing ───────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &App) {
    match app.page {
        Page::Chat => draw_chat(f, app),
        Page::Settings => draw_settings(f, app),
    }
}

fn draw_chat(f: &mut Frame, app: &App) {
    let size = f.area();
    // Clear background to black
    f.render_widget(Clear, size);
    let bg = Paragraph::new("").style(Style::default().bg(Color::Black));
    f.render_widget(bg, size);

    // Layout:  box (entire height - 1)  |  status bar (1 line, outside box)
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(1)])
        .split(size);

    let box_area = outer[0];
    let status_area = outer[1];

    // Input area = 1 line of text inside the box, plus the divider line above it.
    // So from the bottom of the box: border(1) + input_line(1) + divider(1) = 3 lines from bottom.
    // divider_y is relative to box top:  box_area.height - 3
    let input_height: u16 = 1;
    let divider_y = box_area.height.saturating_sub(2 + input_height); // y of ╠═╣ relative to box top

    // Draw double-line box with divider
    let dbox = DoubleBox::new().with_divider(divider_y);
    let inner = dbox.inner(box_area);
    f.render_widget(dbox, box_area);

    // Chat content area: from inner.y to divider absolute y (exclusive)
    let divider_abs_y = box_area.y + divider_y;
    let chat_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: divider_abs_y.saturating_sub(inner.y),
    };

    // Input area: the line below the divider, inside the box
    let input_area = Rect {
        x: inner.x,
        y: divider_abs_y + 1,
        width: inner.width,
        height: input_height,
    };

    // ── Chat / Welcome content ──
    if app.show_welcome && app.messages.is_empty() {
        draw_welcome(f, chat_area);
    } else {
        draw_messages(f, app, chat_area);
    }

    // ── Input line ──
    draw_input(f, app, input_area);

    // ── Status bar (outside the box) ──
    draw_status(f, app, status_area);
}

fn draw_welcome(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // Vertical padding to roughly center
    let logo_height = LOGO.len() as u16 + WELCOME_LINES.len() as u16;
    let pad = area.height.saturating_sub(logo_height) / 2;
    for _ in 0..pad.min(4) {
        lines.push(Line::from(""));
    }

    // ASCII art logo
    for row in LOGO {
        lines.push(Line::from(Span::styled(
            *row,
            Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
        )));
    }

    // Welcome text
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

fn draw_messages(f: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
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
                // system / tool output
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

    // Auto-scroll to bottom, with scroll_offset for manual scrollback
    let total = lines.len() as u16;
    let visible = area.height;
    let max_scroll = total.saturating_sub(visible);
    let scroll = max_scroll.saturating_sub(app.scroll_offset);

    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(p, area);
}

fn draw_input(f: &mut Frame, app: &App, area: Rect) {
    if app.is_thinking {
        let line = Line::from(vec![
            Span::styled("> ", Style::default().fg(BRAND)),
            Span::styled("thinking…", Style::default().fg(DIM).add_modifier(Modifier::ITALIC)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else {
        let line = Line::from(vec![
            Span::styled("> ", Style::default().fg(BRAND).add_modifier(Modifier::BOLD)),
            Span::styled(&*app.input, Style::default().fg(TEXT)),
        ]);
        f.render_widget(Paragraph::new(line), area);

        // Cursor: +2 for "> " prefix
        let cx = area.x + 2 + app.cursor_pos as u16;
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

    let line = Line::from(vec![
        Span::styled(format!(" {} ", provider), Style::default().fg(Color::Black).bg(BRAND)),
        Span::styled(format!(" {} ", model), Style::default().fg(BRAND)),
        Span::styled("│ ", Style::default().fg(DIM)),
        Span::styled(format!("tokens: {}", app.total_tokens), Style::default().fg(DIM)),
        Span::styled(" │ ", Style::default().fg(DIM)),
        Span::styled("Ctrl+S settings  Esc quit", Style::default().fg(DIM)),
        Span::styled(extra, Style::default().fg(Color::Green)),
    ]);
    let p = Paragraph::new(line).style(Style::default().bg(Color::Rgb(15, 15, 20)));
    f.render_widget(p, area);
}

// ── Settings page ─────────────────────────────────────────────────────

fn draw_settings(f: &mut Frame, app: &App) {
    let size = f.area();
    f.render_widget(Clear, size);

    // Center panel
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(10), Constraint::Min(16), Constraint::Percentage(10)])
        .split(size);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(10), Constraint::Min(60), Constraint::Percentage(10)])
        .split(vert[1]);
    let area = horiz[1];

    // Double box for settings too
    let dbox = DoubleBox::new();
    let inner = dbox.inner(area);
    f.render_widget(dbox, area);

    // Title inside box
    let title = Line::from(Span::styled(
        " ⚙  BYOK Settings ",
        Style::default().fg(BRAND).add_modifier(Modifier::BOLD),
    ));
    f.render_widget(Paragraph::new(title), Rect { y: inner.y, height: 1, ..inner });

    // Fields
    let list_area = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: inner.height.saturating_sub(5),
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

    // Help at bottom
    let help_area = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(2),
        width: inner.width,
        height: 2,
    };
    let help = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("↑↓", Style::default().fg(BRAND)),
            Span::styled(" navigate  ", Style::default().fg(DIM)),
            Span::styled("Enter/e", Style::default().fg(BRAND)),
            Span::styled(" edit  ", Style::default().fg(DIM)),
            Span::styled("s", Style::default().fg(BRAND)),
            Span::styled(" save & close  ", Style::default().fg(DIM)),
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
