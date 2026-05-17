use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use perps_funding::FUNDING_LOG_FILE;
use perps_strategy::{decide, Decision, FundingSignal, PortfolioState, Thresholds};
use perps_types::FundingRate;
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

    let thresholds = Thresholds {
        min_apy_to_enter: settings.strategy.min_funding_apy_to_enter,
        max_apy_to_exit: settings.strategy.max_funding_apy_to_exit,
    };
    let max_notional = settings.risk.max_position_usd;
    // Phase 2 dry-run: no live positions yet, so the strategy sees an empty portfolio.
    // Once the executor lands we'll thread real positions in here.
    let portfolio = PortfolioState::default();

    let on_observation = move |fr: &FundingRate| {
        let signal = FundingSignal {
            venue: fr.venue,
            asset: fr.asset.clone(),
            apy: fr.annualized(),
        };
        let decision = decide(&portfolio, &signal, &thresholds, max_notional);
        log_decision(&signal, &decision);
    };

    perps_funding::poll_loop(
        client,
        settings.strategy.assets.clone(),
        interval,
        out_path,
        on_observation,
    )
    .await?;
    Ok(())
}

fn log_decision(signal: &FundingSignal, decision: &Decision) {
    match decision {
        Decision::Open {
            asset,
            side,
            notional_usd,
        } => info!(
            asset = %asset,
            apy = %signal.apy,
            side = ?side,
            notional_usd = %notional_usd,
            kind = "open",
            "dry-run decision"
        ),
        Decision::Close { asset } => info!(
            asset = %asset,
            apy = %signal.apy,
            kind = "close",
            "dry-run decision"
        ),
        Decision::Hold => info!(
            asset = %signal.asset,
            apy = %signal.apy,
            kind = "hold",
            "dry-run decision"
        ),
    }
}
