use clap::Parser;
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
        max_position_usd = %settings.risk.max_position_usd,
        "perps-bot scaffold started — no trading wired yet"
    );

    info!("nothing to do in phase 0; exiting clean");
    Ok(())
}
