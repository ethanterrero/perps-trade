//! Funding-rate polling and signal generation.
//!
//! Phase 1: poll `VenueClient::funding_rate` for each configured asset on a fixed cadence,
//! log observations, and persist to JSONL for later analysis.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use perps_types::FundingRate;
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
pub async fn poll_loop(
    client: Arc<dyn VenueClient>,
    assets: Vec<String>,
    interval: Duration,
    out_path: PathBuf,
) -> anyhow::Result<()> {
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
                poll_once(client.as_ref(), &assets, &out_path).await;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, exiting poll loop");
                return Ok(());
            }
        }
    }
}

async fn poll_once(client: &dyn VenueClient, assets: &[String], out_path: &Path) {
    for asset in assets {
        match client.funding_rate(asset).await {
            Ok(fr) => {
                let annualized = fr.annualized();
                info!(
                    asset = %fr.asset,
                    rate = %fr.rate,
                    interval_hours = fr.interval_hours,
                    annualized = %annualized,
                    "funding observed"
                );
                let obs = Observation { funding: &fr, annualized };
                if let Err(e) = append_jsonl(out_path, &obs).await {
                    error!(error = %e, asset = %asset, "failed to write observation");
                }
            }
            Err(e) => {
                warn!(error = %e, asset = %asset, "funding fetch failed");
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
