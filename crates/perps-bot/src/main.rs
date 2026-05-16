use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use perps_funding::FUNDING_LOG_FILE;
use perps_venues::hyperliquid::HyperliquidClient;
use perps_venues::VenueClient;
use tracing::info;

mod stats;

#[derive(Parser, Debug)]
#[command(name = "perps-bot", about = "Delta-neutral funding-rate harvester")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the funding-rate poll loop (Phase 1 default).
    Run(RunArgs),
    /// Summarize observations recorded in state/funding.jsonl.
    Stats(StatsArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Path to config directory.
    #[arg(long, default_value = "config")]
    config_dir: String,
}

#[derive(Parser, Debug)]
struct StatsArgs {
    /// Directory containing the funding JSONL log.
    #[arg(long, default_value = "state")]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run(RunArgs {
        config_dir: "config".into(),
    })) {
        Command::Run(args) => run(args).await,
        Command::Stats(args) => stats::run(&args.state_dir),
    }
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let settings = perps_config::load(&args.config_dir)?;

    info!(
        venue_url = %settings.venue.hyperliquid.api_url,
        testnet = settings.venue.hyperliquid.testnet,
        assets = ?settings.strategy.assets,
        interval_secs = settings.strategy.decision_interval_seconds,
        state_dir = %settings.telemetry.state_dir,
        "perps-bot starting funding poller"
    );

    let client: Arc<dyn VenueClient> =
        Arc::new(HyperliquidClient::new(&settings.venue.hyperliquid.api_url));
    let out_path = PathBuf::from(&settings.telemetry.state_dir).join(FUNDING_LOG_FILE);
    let interval = Duration::from_secs(settings.strategy.decision_interval_seconds);

    perps_funding::poll_loop(client, settings.strategy.assets.clone(), interval, out_path).await?;
    Ok(())
}
