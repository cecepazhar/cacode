use anyhow::Result;
use clap::{Parser, Subcommand};

use cacode_core::commands;

mod config;
mod tui;

#[derive(Parser)]
#[command(name = "cacode", version, about = "Full‑lifecycle sovereign Rust CLI & chat‑agent")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Commands>,
    /// Headless: run one task and print the result (use "-" to read the prompt from stdin)
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    print: Option<String>,
    /// Headless output: text | json | stream-json
    #[arg(long, default_value = "text")]
    output_format: String,
    /// ask | auto-edit | yolo | read-only | plan
    #[arg(long, env = "CACODE_PERMISSION_MODE")]
    permission_mode: Option<String>,
    /// Max ReAct steps for this run
    #[arg(long)]
    max_steps: Option<usize>,
    /// Continue the most recent session of this directory
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a saved session by id (see `cacode sessions`)
    #[arg(short = 'r', long)]
    resume: Option<String>,
    /// Override the model of the detected provider
    #[arg(long)]
    model: Option<String>,
    /// Trust the current directory (enables project hooks and .mcp.json)
    #[arg(long)]
    trust_workspace: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Show the about page (project description, author, license)
    About,
    /// Show the billing information (pricing tiers, cost per model)
    Billing,
    /// Run the interactive chat agent (REPL mode, non-TUI)
    Chat,
    /// Token‑usage proxy (starts the local proxy server)
    Monitor {
        #[arg(long, env = "REAL_ENDPOINT")]
        endpoint: String,
        /// Extra upstream endpoints tried on connect error / 429 / 5xx (repeatable or comma-separated)
        #[arg(long = "fallback", env = "FALLBACK_ENDPOINTS", value_delimiter = ',')]
        fallback: Vec<String>,
        #[arg(short, long, env = "COST_TABLE")]
        costs: Option<String>,
        #[arg(long, env = "MONITOR_PORT", default_value = "8000")]
        port: Option<u16>,
        #[arg(long, env = "MAX_RPM", default_value = "120")]
        max_rpm: Option<usize>,
    },
    /// List saved sessions (this directory, or --all)
    Sessions {
        #[arg(long)]
        all: bool,
    },
    /// Show collected token‑usage statistics
    Report {
        #[arg(short, long, default_value = "table")]
        format: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let cli = Cli::parse();

    // Config is loaded and exported to the environment BEFORE the async
    // runtime starts any thread: `std::env::set_var` is only sound while the
    // process is single-threaded. Agents receive credentials explicitly.
    let config = cacode_core::config::CaCodeConfig::load().unwrap_or_default();
    config.apply_to_env();

    if cli.trust_workspace {
        cacode_core::trust::trust(&std::env::current_dir()?)?;
    }
    let mode = match cli.permission_mode.as_deref() {
        Some(m) => Some(cacode_core::permission::PermissionMode::parse(m).ok_or_else(|| anyhow::anyhow!("unknown --permission-mode '{}'", m))?),
        None => None,
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        if let Some(prompt) = cli.print {
            use cacode_core::commands::headless::{run, HeadlessOptions, OutputFormat};
            let code = run(HeadlessOptions {
                prompt,
                format: OutputFormat::parse(&cli.output_format)?,
                permission_mode: mode,
                max_steps: cli.max_steps,
                continue_last: cli.continue_last,
                resume: cli.resume,
                model: cli.model,
            })
            .await?;
            std::process::exit(code);
        }
        match cli.cmd {
            None => {
                let store = cacode_core::sessions::SessionStore::open_default();
                let resume = match (&cli.resume, cli.continue_last) {
                    (Some(id), _) => Some(store.load(id)?),
                    (None, true) => store.latest_for(&std::env::current_dir()?.canonicalize()?),
                    _ => None,
                };
                tui::run_tui(tui::TuiOptions { resume, permission_mode: mode, model: cli.model }).await?
            }
            Some(Commands::Sessions { all }) => cacode_core::commands::headless::list_sessions(all)?,
            Some(Commands::About) => commands::about::run()?,
            Some(Commands::Billing) => commands::billing::run()?,
            Some(Commands::Chat) => commands::chat::run().await?,
            Some(Commands::Monitor { endpoint, fallback, costs, port, max_rpm }) => {
                commands::monitor::run_with_fallbacks(endpoint, fallback, costs, port, max_rpm).await?;
            }
            Some(Commands::Report { format }) => commands::report::run(format)?,
        }
        Ok(())
    })
}
