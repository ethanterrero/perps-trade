//! Funding-rate polling and signal generation.
//!
//! Phase 1+: poll `VenueClient::market_snapshot` for each configured asset on a
//! fixed cadence, log the funding piece, persist it to JSONL for later analysis,
//! and hand the full snapshot (funding + mid price) to the caller's callback so
//! downstream layers (strategy, executor) can react.

/// File name (within `state_dir`) for the append-only funding-rate log.
pub const FUNDING_LOG_FILE: &str = "funding.jsonl";

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use perps_types::{FundingRate, MarketSnapshot};
use perps_venues::VenueClient;
use rust_decimal::Decimal;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tracing::{error, info, warn};

#[derive(Serialize)]
struct Observation<'a> {
    #[serde(flatten)]
    funding: &'a FundingRate,
    annualized: Decimal,
}

/// Run the poll loop until ctrl-c. One tick fires immediately, then every `interval`.
/// Per-asset errors are logged and skipped; the loop only exits on signal.
///
/// `on_snapshot` is invoked synchronously after each successful fetch is
/// persisted. It's the seam where higher layers (strategy, executor) react to
/// snapshots without `perps-funding` having to depend on them. Pass a no-op
/// closure if you only care about persistence. The full `MarketSnapshot` is
/// passed (not just the funding piece) so the caller has the mid price for
/// fill simulation without a second venue round-trip.
pub async fn poll_loop<F>(
    client: Arc<dyn VenueClient>,
    assets: Vec<String>,
    interval: Duration,
    out_path: PathBuf,
    on_snapshot: F,
) -> anyhow::Result<()>
where
    F: Fn(&MarketSnapshot) + Send + Sync + 'static,
{
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    info!(
        path = %out_path.display(),
        interval_secs = interval.as_secs(),
        assets = ?assets,
        "funding poll loop starting"
    );

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                poll_once(client.as_ref(), &assets, &out_path, &on_snapshot).await;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, exiting poll loop");
                return Ok(());
            }
        }
    }
}

async fn poll_once<F>(
    client: &dyn VenueClient,
    assets: &[String],
    out_path: &Path,
    on_snapshot: &F,
) where
    F: Fn(&MarketSnapshot),
{
    for asset in assets {
        match client.market_snapshot(asset).await {
            Ok(snapshot) => {
                let fr = &snapshot.funding;
                let annualized = fr.annualized();
                info!(
                    asset = %fr.asset,
                    rate = %fr.rate,
                    interval_hours = fr.interval_hours,
                    annualized = %annualized,
                    mid_price = %snapshot.mid_price,
                    "funding observed"
                );
                let obs = Observation { funding: fr, annualized };
                if let Err(e) = append_jsonl(out_path, &obs).await {
                    error!(error = %e, asset = %asset, "failed to write observation");
                }
                on_snapshot(&snapshot);
            }
            Err(e) => {
                warn!(error = %e, asset = %asset, "market snapshot fetch failed");
            }
        }
    }
}

async fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let line = serde_json::to_string(value)?;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(line.as_bytes()).await?;
    f.write_all(b"\n").await?;
    f.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use perps_types::Venue;
    use rust_decimal_macros::dec;

    #[test]
    fn observation_serializes_with_annualized() {
        let fr = FundingRate {
            venue: Venue::Hyperliquid,
            asset: "BTC".into(),
            rate: dec!(0.0001),
            interval_hours: 1,
            observed_at: chrono::Utc
                .with_ymd_and_hms(2026, 5, 14, 0, 0, 0)
                .unwrap(),
        };
        let annualized = fr.annualized();
        let obs = Observation { funding: &fr, annualized };
        let v: serde_json::Value = serde_json::to_value(&obs).unwrap();
        assert_eq!(v["asset"], "BTC");
        assert_eq!(v["venue"], "hyperliquid");
        assert_eq!(v["interval_hours"], 1);
        // rust_decimal `serde-with-str` => string form.
        assert_eq!(v["rate"], "0.0001");
        assert_eq!(v["annualized"], "0.8760");
        assert_eq!(v["observed_at"], "2026-05-14T00:00:00Z");
    }
}
