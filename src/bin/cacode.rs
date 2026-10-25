use clap::{Parser, Subcommand};
use anyhow::Result;

use cacode::commands;

#[derive(Parser)]
#[command(name = "cacode", version, about = "Full‑lifecycle sovereign Rust CLI & chat‑agent")]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show the about page (project description, author, license)
    About,
    /// Show the billing information (pricing tiers, cost per model)
    Billing,
    /// Run the interactive chat agent (core functionality – omitted here for brevity)
    Chat,
    /// Token‑usage proxy (starts the local proxy server)
    Monitor {
        #[arg(long, env = "REAL_ENDPOINT")]
        endpoint: String,
        #[arg(short, long, env = "COST_TABLE")]
        costs: Option<String>,
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
        Commands::About => commands::about::run()? ,
        Commands::Billing => commands::billing::run()? ,
        Commands::Chat => commands::chat::run().await?,
        Commands::Monitor { endpoint, costs } => {
            commands::monitor::run(endpoint, costs).await?;
        }
        Commands::Report { format } => commands::report::run(format)?,
    }
    Ok(())
}
