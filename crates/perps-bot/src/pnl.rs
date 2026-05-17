//! PnL attribution for paper-trade runs.
//!
//! Reads `fills.jsonl` to reconstruct closed trades (Open paired with Close on
//! the same asset) and still-open positions (Open with no matching Close yet),
//! then reads `funding.jsonl` to integrate funding earned/paid over each
//! position's lifetime. Realized + unrealized + funding gives a per-asset
//! attribution suitable for end-of-day digests.
//!
//! Approximations:
//!   - Funding is integrated as a step function: rate at observation O_i applies
//!     until the next observation O_{i+1}. For the pre-first and post-last
//!     segments we use the closest available rate. This is good enough for
//!     paper-trade ballparks; real funding is paid in discrete events per
//!     funding period, but we don't model that yet.
//!   - Unrealized PnL uses the latest mid price observed for that asset.
//!   - No fees (Phase 2 dry-run is fee-free).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Utc};
use perps_executor::{Fill, FillIntent, FILLS_LOG_FILE};
use perps_funding::FUNDING_LOG_FILE;
use perps_types::Side;
use rust_decimal::Decimal;
use serde::Deserialize;

/// One row of the parsed `funding.jsonl` log: the underlying `FundingRate` fields,
/// plus the `annualized` + `mid_price` fields the bot writes alongside. Older
/// log entries from before mid_price was added will fail to parse here; logs
/// from PR #7 onward include it.
#[derive(Deserialize, Debug, Clone)]
pub struct LoggedFunding {
    pub asset: String,
    pub rate: Decimal,
    pub interval_hours: u8,
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub mid_price: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct ClosedTrade {
    pub asset: String,
    /// Side of the original Open (the position's side).
    pub side: Side,
    pub open: Fill,
    pub close: Fill,
}

impl ClosedTrade {
    /// Realized price PnL in USD. Positive = profit.
    pub fn realized_pnl(&self) -> Decimal {
        let entry = self.open.price;
        let exit = self.close.price;
        let size = self.open.size;
        match self.side {
            Side::Long => (exit - entry) * size,
            Side::Short => (entry - exit) * size,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenTrade {
    pub asset: String,
    pub side: Side,
    pub open: Fill,
}

impl OpenTrade {
    /// Unrealized PnL in USD against the current mid. Same sign convention as
    /// realized.
    pub fn unrealized_pnl(&self, current_mid: Decimal) -> Decimal {
        let entry = self.open.price;
        let size = self.open.size;
        match self.side {
            Side::Long => (current_mid - entry) * size,
            Side::Short => (entry - current_mid) * size,
        }
    }
}

pub fn read_fills(state_dir: &Path) -> anyhow::Result<Vec<Fill>> {
    let path = state_dir.join(FILLS_LOG_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path)?;
    let reader = BufReader::new(file);
    let mut fills = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(fill) = serde_json::from_str::<Fill>(&line) {
            fills.push(fill);
        }
    }
    fills.sort_by_key(|f| f.filled_at);
    Ok(fills)
}

pub fn read_funding(state_dir: &Path) -> anyhow::Result<Vec<LoggedFunding>> {
    let path = state_dir.join(FUNDING_LOG_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path)?;
    let reader = BufReader::new(file);
    let mut obs = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(f) = serde_json::from_str::<LoggedFunding>(&line) {
            obs.push(f);
        }
    }
    obs.sort_by_key(|o| o.observed_at);
    Ok(obs)
}

/// Pair Open fills with their corresponding Close fills per asset. Returns
/// `(closed_trades, still_open)`. Assumes:
///   - Fills are time-ordered (caller sorts).
///   - Each Open is closed by the next Close on the same asset. Multiple Opens
///     on the same asset before a Close shouldn't happen given the portfolio
///     tracker, but if they do, only the latest Open is paired (older Opens
///     are dropped silently).
///   - A Close with no preceding Open is dropped silently.
pub fn pair_fills(fills: &[Fill]) -> (Vec<ClosedTrade>, Vec<OpenTrade>) {
    let mut current_open: HashMap<String, Fill> = HashMap::new();
    let mut closed = Vec::new();

    for fill in fills {
        let asset = fill.asset.to_ascii_uppercase();
        match fill.intent {
            FillIntent::Open => {
                current_open.insert(asset, fill.clone());
            }
            FillIntent::Close => {
                if let Some(open) = current_open.remove(&asset) {
                    closed.push(ClosedTrade {
                        asset: open.asset.clone(),
                        side: open.side,
                        open,
                        close: fill.clone(),
                    });
                }
            }
        }
    }

    let still_open: Vec<OpenTrade> = current_open
        .into_values()
        .map(|fill| OpenTrade {
            asset: fill.asset.clone(),
            side: fill.side,
            open: fill,
        })
        .collect();

    (closed, still_open)
}

/// Step-integrate funding accrual over `[from, to]` for a position with the
/// given side and notional. Returns USD; positive = the position earned funding,
/// negative = paid it.
///
/// Sign convention follows Hyperliquid: positive funding rate means longs pay
/// shorts, so a Short with positive rate earns; a Long with positive rate pays.
pub fn accrued_funding(
    asset: &str,
    side: Side,
    notional: Decimal,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    observations: &[LoggedFunding],
) -> Decimal {
    if to <= from {
        return Decimal::ZERO;
    }
    let mut relevant: Vec<&LoggedFunding> = observations
        .iter()
        .filter(|o| o.asset.eq_ignore_ascii_case(asset))
        .filter(|o| o.observed_at >= from && o.observed_at <= to)
        .collect();
    relevant.sort_by_key(|o| o.observed_at);

    if relevant.is_empty() {
        // Fall back to the most recent observation before `from`, if any.
        if let Some(prior) = observations
            .iter()
            .filter(|o| o.asset.eq_ignore_ascii_case(asset))
            .filter(|o| o.observed_at < from)
            .max_by_key(|o| o.observed_at)
        {
            return apply_rate(side, notional, prior.rate, prior.interval_hours, from, to);
        }
        return Decimal::ZERO;
    }

    let mut total = Decimal::ZERO;
    // Pre-first segment: from open to first observation, using first rate.
    let first = relevant[0];
    if first.observed_at > from {
        total += apply_rate(side, notional, first.rate, first.interval_hours, from, first.observed_at);
    }
    // Between-observations segments.
    for window in relevant.windows(2) {
        let (a, b) = (window[0], window[1]);
        total += apply_rate(side, notional, a.rate, a.interval_hours, a.observed_at, b.observed_at);
    }
    // Post-last segment.
    let last = *relevant.last().unwrap();
    if last.observed_at < to {
        total += apply_rate(side, notional, last.rate, last.interval_hours, last.observed_at, to);
    }
    total
}

fn apply_rate(
    side: Side,
    notional: Decimal,
    rate: Decimal,
    interval_hours: u8,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Decimal {
    let ms = (end - start).num_milliseconds();
    if ms <= 0 {
        return Decimal::ZERO;
    }
    let duration_hours = Decimal::from(ms) / Decimal::from(3_600_000);
    let interval = Decimal::from(interval_hours.max(1));
    let magnitude = rate * notional * duration_hours / interval;
    match side {
        Side::Short => magnitude,
        Side::Long => -magnitude,
    }
}

/// Latest observed mid price per asset (case-folded uppercase key).
pub fn latest_mids(observations: &[LoggedFunding]) -> HashMap<String, Decimal> {
    let mut latest: HashMap<String, (DateTime<Utc>, Decimal)> = HashMap::new();
    for o in observations {
        if let Some(mid) = o.mid_price {
            let key = o.asset.to_ascii_uppercase();
            latest
                .entry(key)
                .and_modify(|(t, m)| {
                    if o.observed_at > *t {
                        *t = o.observed_at;
                        *m = mid;
                    }
                })
                .or_insert((o.observed_at, mid));
        }
    }
    latest.into_iter().map(|(k, (_, m))| (k, m)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use perps_types::Venue;
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    fn fill(asset: &str, intent: FillIntent, side: Side, price: Decimal, size: Decimal, secs: i64) -> Fill {
        Fill {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            side,
            size,
            price,
            notional_usd: price * size,
            fees_usd: Decimal::ZERO,
            filled_at: Utc.timestamp_opt(secs, 0).unwrap(),
            intent,
            order_id: Uuid::nil(),
        }
    }

    fn obs(asset: &str, rate: Decimal, secs: i64) -> LoggedFunding {
        LoggedFunding {
            asset: asset.into(),
            rate,
            interval_hours: 1,
            observed_at: Utc.timestamp_opt(secs, 0).unwrap(),
            mid_price: Some(dec!(100)),
        }
    }

    #[test]
    fn pairs_open_then_close() {
        let fills = vec![
            fill("BTC", FillIntent::Open, Side::Short, dec!(100), dec!(1), 0),
            fill("BTC", FillIntent::Close, Side::Long, dec!(98), dec!(1), 100),
        ];
        let (closed, open) = pair_fills(&fills);
        assert_eq!(closed.len(), 1);
        assert!(open.is_empty());
        assert_eq!(closed[0].realized_pnl(), dec!(2)); // Short opened at 100, closed at 98 → +2
    }

    #[test]
    fn unmatched_open_becomes_open_trade() {
        let fills = vec![fill("BTC", FillIntent::Open, Side::Short, dec!(100), dec!(1), 0)];
        let (closed, open) = pair_fills(&fills);
        assert!(closed.is_empty());
        assert_eq!(open.len(), 1);
        // Unrealized: mid 102 vs entry 100, Short → -2.
        assert_eq!(open[0].unrealized_pnl(dec!(102)), dec!(-2));
    }

    #[test]
    fn pairs_per_asset_independently() {
        let fills = vec![
            fill("BTC", FillIntent::Open, Side::Short, dec!(100), dec!(1), 0),
            fill("ETH", FillIntent::Open, Side::Long, dec!(50), dec!(2), 5),
            fill("BTC", FillIntent::Close, Side::Long, dec!(99), dec!(1), 10),
            // ETH stays open.
        ];
        let (closed, open) = pair_fills(&fills);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].asset, "BTC");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].asset, "ETH");
    }

    #[test]
    fn realized_pnl_long_profit() {
        let fills = vec![
            fill("BTC", FillIntent::Open, Side::Long, dec!(100), dec!(2), 0),
            fill("BTC", FillIntent::Close, Side::Short, dec!(110), dec!(2), 100),
        ];
        let (closed, _) = pair_fills(&fills);
        assert_eq!(closed[0].realized_pnl(), dec!(20)); // (110-100) * 2
    }

    #[test]
    fn funding_accrual_short_with_positive_rate_earns() {
        // Rate 0.01 per 1-hour period, notional $1000, 1 hour held.
        // Expected: 0.01 * 1000 * 1 = $10 earned for Short.
        let observations = vec![obs("BTC", dec!(0.01), 0)];
        let from = Utc.timestamp_opt(0, 0).unwrap();
        let to = Utc.timestamp_opt(3600, 0).unwrap();
        let pnl = accrued_funding("BTC", Side::Short, dec!(1000), from, to, &observations);
        assert_eq!(pnl, dec!(10));
    }

    #[test]
    fn funding_accrual_long_with_positive_rate_pays() {
        let observations = vec![obs("BTC", dec!(0.01), 0)];
        let from = Utc.timestamp_opt(0, 0).unwrap();
        let to = Utc.timestamp_opt(3600, 0).unwrap();
        let pnl = accrued_funding("BTC", Side::Long, dec!(1000), from, to, &observations);
        assert_eq!(pnl, dec!(-10)); // Long pays positive funding
    }

    #[test]
    fn funding_accrual_step_integrates_changing_rate() {
        // Two observations: rate 0.01 then 0.02. Each applies for 30 min over a 1-hour hold.
        // Expected: 0.01 * 1000 * 0.5 + 0.02 * 1000 * 0.5 = 5 + 10 = $15 for Short.
        let observations = vec![obs("BTC", dec!(0.01), 0), obs("BTC", dec!(0.02), 1800)];
        let from = Utc.timestamp_opt(0, 0).unwrap();
        let to = Utc.timestamp_opt(3600, 0).unwrap();
        let pnl = accrued_funding("BTC", Side::Short, dec!(1000), from, to, &observations);
        assert_eq!(pnl, dec!(15));
    }

    #[test]
    fn funding_accrual_ignores_other_assets() {
        let observations = vec![obs("ETH", dec!(0.01), 0)];
        let from = Utc.timestamp_opt(0, 0).unwrap();
        let to = Utc.timestamp_opt(3600, 0).unwrap();
        let pnl = accrued_funding("BTC", Side::Short, dec!(1000), from, to, &observations);
        assert_eq!(pnl, Decimal::ZERO);
    }

    #[test]
    fn latest_mids_takes_most_recent_per_asset() {
        let obs = vec![
            LoggedFunding {
                asset: "BTC".into(),
                rate: dec!(0.0001),
                interval_hours: 1,
                observed_at: Utc.timestamp_opt(0, 0).unwrap(),
                mid_price: Some(dec!(60000)),
            },
            LoggedFunding {
                asset: "BTC".into(),
                rate: dec!(0.0001),
                interval_hours: 1,
                observed_at: Utc.timestamp_opt(100, 0).unwrap(),
                mid_price: Some(dec!(60500)),
            },
            LoggedFunding {
                asset: "ETH".into(),
                rate: dec!(0.0001),
                interval_hours: 1,
                observed_at: Utc.timestamp_opt(50, 0).unwrap(),
                mid_price: Some(dec!(3000)),
            },
        ];
        let mids = latest_mids(&obs);
        assert_eq!(mids.get("BTC"), Some(&dec!(60500)));
        assert_eq!(mids.get("ETH"), Some(&dec!(3000)));
    }
}
