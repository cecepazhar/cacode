use clap::{Parser, Subcommand};
use anyhow::Result;

use cacode_core::commands;

mod config;
mod tui;

#[derive(Parser)]
#[command(name = "cacode", version, about = "Full‑lifecycle sovereign Rust CLI & chat‑agent")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Commands>,
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
        #[arg(short, long, env = "COST_TABLE")]
        costs: Option<String>,
        #[arg(long, env = "MONITOR_PORT", default_value = "8000")]
        port: Option<u16>,
        #[arg(long, env = "MAX_RPM", default_value = "120")]
        max_rpm: Option<usize>,
    },
    /// Show collected token‑usage statistics
    Report {
        #[arg(short, long, default_value = "table")]
        format: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            // No subcommand → launch TUI chat interface
            tui::run_tui().await?;
        }
        Some(Commands::About) => commands::about::run()?,
        Some(Commands::Billing) => commands::billing::run()?,
        Some(Commands::Chat) => commands::chat::run().await?,
        Some(Commands::Monitor { endpoint, costs, port, max_rpm }) => {
            commands::monitor::run(endpoint, costs, port, max_rpm).await?;
        }
        Some(Commands::Report { format }) => commands::report::run(format)?,
    }
    Ok(())
}
