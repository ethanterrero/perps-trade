use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use perps_venues::hyperliquid::HyperliquidClient;
use perps_venues::VenueClient;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "perps-bot", about = "Delta-neutral funding-rate harvester")]
struct Args {
    /// Path to config directory.
    #[arg(long, default_value = "config")]
    config_dir: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let args = Args::parse();
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
    let out_path = PathBuf::from(&settings.telemetry.state_dir).join("funding.jsonl");
    let interval = Duration::from_secs(settings.strategy.decision_interval_seconds);

    perps_funding::poll_loop(client, settings.strategy.assets.clone(), interval, out_path).await?;
    Ok(())
}
