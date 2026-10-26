//! `perps-bot flatten` — emergency kill-switch. Reads `fills.jsonl`, finds
//! still-open legs (perp and spot), fetches a fresh mid for each, and appends
//! a Close fill per leg to bring the bot's portfolio to flat.
//!
//! **Run with the live daemon stopped.** If both the daemon and `flatten` are
//! writing to `fills.jsonl` simultaneously the resulting state is racy and
//! the bot may "see" positions that no longer exist. Suggested workflow on
//! macOS: `launchctl unload …com.perps-trade.bot.plist`, then run `flatten`.

use std::path::Path;
use std::sync::Arc;

use perps_executor::{simulate_fill, spot_fill_price, FillIntent, FILLS_LOG_FILE};
use perps_types::{Instrument, Order};
use perps_venues::hyperliquid::HyperliquidClient;
use perps_venues::VenueClient;
use tracing::{info, warn};
use uuid::Uuid;

use crate::pnl;

pub async fn run(config_dir: &str, state_dir: &Path) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let settings = perps_config::load(config_dir)?;
    let fills = pnl::read_fills(state_dir)?;
    let (_closed, open_trades) = pnl::pair_fills(&fills);

    if open_trades.is_empty() {
        println!("nothing to flatten — no open legs in {}", state_dir.display());
        return Ok(());
    }

    println!(
        "flattening {} open leg(s) via {}",
        open_trades.len(),
        settings.venue.hyperliquid.api_url
    );

    let client: Arc<dyn VenueClient> = Arc::new(
        HyperliquidClient::new(&settings.venue.hyperliquid.api_url)
            .with_spot_tokens(&settings.venue.hyperliquid.spot_tokens),
    );
    let fills_path = state_dir.join(FILLS_LOG_FILE);

    let mut closed_ok = 0;
    let mut errored = Vec::new();

    for trade in &open_trades {
        let asset = &trade.asset;
        let mid = match client.market_snapshot(asset).await {
            Ok(snap) => match trade.instrument {
                Instrument::Perp => snap.mid_price,
                Instrument::Spot => spot_fill_price(snap.mid_price, snap.spot_mid_price)
                    .unwrap_or_else(|| {
                        warn!(asset = %asset, "flatten: no sane spot mid, closing spot leg at perp mid");
                        snap.mid_price
                    }),
            },
            Err(e) => {
                warn!(asset = %asset, error = %e, "could not fetch mid for flatten");
                errored.push((asset.clone(), format!("{e}")));
                continue;
            }
        };
        let order = Order {
            venue: trade.open.venue,
            asset: asset.clone(),
            instrument: trade.instrument,
            side: trade.side.flip(),
            size: trade.open.size,
            limit_price: None,
            reduce_only: true,
            client_id: Uuid::new_v4(),
        };
        let fee_rate = settings.fees.taker_rate(trade.instrument);
        let fill = simulate_fill(&order, mid, FillIntent::Close, fee_rate);
        info!(
            asset = %fill.asset,
            instrument = ?fill.instrument,
            side = ?fill.side,
            size = %fill.size,
            price = %fill.price,
            notional_usd = %fill.notional_usd,
            intent = "close",
            "flatten: simulated close fill"
        );
        match perps_executor::append(&fills_path, &fill) {
            Ok(()) => closed_ok += 1,
            Err(e) => {
                warn!(asset = %asset, error = %e, "could not persist close fill");
                errored.push((asset.clone(), format!("persist: {e}")));
            }
        }
    }

    println!(
        "flatten complete: closed {} of {} leg(s)",
        closed_ok,
        open_trades.len()
    );
    if !errored.is_empty() {
        println!("errors:");
        for (asset, err) in &errored {
            println!("  {asset}: {err}");
        }
        anyhow::bail!("{} of {} legs failed to flatten", errored.len(), open_trades.len());
    }

    Ok(())
}
