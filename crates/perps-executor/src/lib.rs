//! Order placement, fill tracking, and notional-balance rebalancing.
//!
//! Two modes:
//!   - DryRun: simulate fills at venue mid, no orders sent (Phase 2 paper trading).
//!   - Live:   send signed orders to the venue (Phase 4+).
//!
//! Phase 2 lives entirely in the DryRun path: [`simulate_fill`] produces a [`Fill`]
//! at the supplied mid price, charging taker fees at the rate the caller passes
//! (from `perps_types::FeeSchedule`). Slippage is still not modelled on fills;
//! the backtest charges it separately.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use perps_types::{Instrument, Order, Side, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// File name (within `state_dir`) for the append-only fills log.
pub const FILLS_LOG_FILE: &str = "fills.jsonl";

/// Did this fill open a new position or close an existing one? Recorded explicitly
/// so the portfolio tracker can apply it correctly without re-deriving intent from
/// side + existing-position state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FillIntent {
    Open,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub venue: Venue,
    pub asset: String,
    /// Perp or spot leg. Fills logged before the hedge leg existed are perp.
    #[serde(default)]
    pub instrument: Instrument,
    /// Side of the order that produced this fill. For an Open this is the new
    /// position's side; for a Close it's the side opposite the position being
    /// closed.
    pub side: Side,
    /// Size in base units (e.g. BTC).
    pub size: Decimal,
    /// Filled price in USD per base unit.
    pub price: Decimal,
    /// `size * price`. Stored explicitly so consumers don't have to re-multiply
    /// and risk a Decimal-scale mismatch.
    pub notional_usd: Decimal,
    /// Total fees paid in USD (`notional_usd * fee_rate` in dry-run).
    pub fees_usd: Decimal,
    pub filled_at: DateTime<Utc>,
    pub intent: FillIntent,
    /// `client_id` of the originating Order — lets us link Fills back to the
    /// decision that produced them.
    pub order_id: uuid::Uuid,
}

/// Dry-run fill simulation: use the order's limit price if specified, otherwise
/// the supplied `mid_price`. No partial fills, no slippage; fees are
/// `notional * fee_rate`. The intent is supplied by the caller (the strategy/bot
/// knows whether the order is opening or closing a position; the executor
/// doesn't need to figure it out).
pub fn simulate_fill(order: &Order, mid_price: Decimal, intent: FillIntent, fee_rate: Decimal) -> Fill {
    let price = order.limit_price.unwrap_or(mid_price);
    let notional_usd = order.size * price;
    Fill {
        venue: order.venue,
        asset: order.asset.clone(),
        instrument: order.instrument,
        side: order.side,
        size: order.size,
        price,
        notional_usd,
        fees_usd: notional_usd * fee_rate,
        filled_at: Utc::now(),
        intent,
        order_id: order.client_id,
    }
}

/// Largest gap between spot and perp mids we'll accept as a real hedge price.
/// Majors trade within a few bps of each other; anything wider is a broken
/// quote (testnet spot books are mostly empty or stale).
pub const MAX_SPOT_PERP_GAP: Decimal = Decimal::from_parts(2, 0, 0, false, 2);

/// Price for the spot leg's fill: the spot mid when it's within
/// [`MAX_SPOT_PERP_GAP`] of the perp mid, otherwise `None` so the caller can
/// decide whether to refuse or fall back.
pub fn spot_fill_price(perp_mid: Decimal, spot_mid: Option<Decimal>) -> Option<Decimal> {
    let spot = spot_mid.filter(|s| *s > Decimal::ZERO)?;
    if perp_mid <= Decimal::ZERO {
        return None;
    }
    let gap = ((spot - perp_mid) / perp_mid).abs();
    (gap <= MAX_SPOT_PERP_GAP).then_some(spot)
}

/// Sync append. Fills are infrequent (one per Open or Close decision, at most
/// once per asset per poll tick) and the line is ~250 bytes, so a blocking
/// write inside the poll-loop callback is negligible.
pub fn append(path: &Path, fill: &Fill) -> std::io::Result<()> {
    let line = serde_json::to_string(fill).expect("Fill serializes infallibly");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn order(asset: &str, side: Side, size: Decimal) -> Order {
        Order {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            instrument: Instrument::Perp,
            side,
            size,
            limit_price: None,
            reduce_only: false,
            client_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn market_order_fills_at_mid() {
        let o = order("BTC", Side::Short, dec!(0.01));
        let fill = simulate_fill(&o, dec!(60000), FillIntent::Open, dec!(0.00045));
        assert_eq!(fill.price, dec!(60000));
        assert_eq!(fill.size, dec!(0.01));
        assert_eq!(fill.notional_usd, dec!(600));
        assert_eq!(fill.fees_usd, dec!(0.27));
        assert_eq!(fill.intent, FillIntent::Open);
        assert_eq!(fill.order_id, o.client_id);
    }

    #[test]
    fn limit_order_fills_at_limit_not_mid() {
        let mut o = order("BTC", Side::Long, dec!(0.05));
        o.limit_price = Some(dec!(59500));
        let fill = simulate_fill(&o, dec!(60000), FillIntent::Open, Decimal::ZERO);
        assert_eq!(fill.price, dec!(59500));
        assert_eq!(fill.notional_usd, dec!(0.05) * dec!(59500));
    }

    #[test]
    fn close_intent_carries_through() {
        let o = order("ETH", Side::Long, dec!(1));
        let fill = simulate_fill(&o, dec!(3000), FillIntent::Close, Decimal::ZERO);
        assert_eq!(fill.intent, FillIntent::Close);
        // Side records the closing order's direction (Long to close a Short).
        assert_eq!(fill.side, Side::Long);
    }

    #[test]
    fn spot_leg_carries_instrument_and_its_own_fee() {
        let mut o = order("BTC", Side::Long, dec!(0.01));
        o.instrument = Instrument::Spot;
        let fill = simulate_fill(&o, dec!(60000), FillIntent::Open, dec!(0.0007));
        assert_eq!(fill.instrument, Instrument::Spot);
        assert_eq!(fill.fees_usd, dec!(0.42));
    }

    #[test]
    fn spot_fill_price_accepts_close_quotes_only() {
        assert_eq!(spot_fill_price(dec!(100), Some(dec!(100.5))), Some(dec!(100.5)));
        assert_eq!(spot_fill_price(dec!(100), Some(dec!(102))), Some(dec!(102)));
        // Testnet-style broken quote: UETH spot at 83 vs perp at 2300.
        assert_eq!(spot_fill_price(dec!(2300), Some(dec!(83))), None);
        assert_eq!(spot_fill_price(dec!(100), None), None);
        assert_eq!(spot_fill_price(dec!(100), Some(Decimal::ZERO)), None);
    }

    #[test]
    fn pre_hedge_fill_lines_deserialize_as_perp() {
        let line = r#"{"venue":"hyperliquid","asset":"BTC","side":"short","size":"0.01",
            "price":"60000","notional_usd":"600","fees_usd":"0","filled_at":"2026-05-17T00:00:00Z",
            "intent":"open","order_id":"00000000-0000-0000-0000-000000000000"}"#;
        let fill: Fill = serde_json::from_str(line).unwrap();
        assert_eq!(fill.instrument, Instrument::Perp);
    }

    #[test]
    fn append_roundtrips_through_jsonl() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("fills.jsonl");

        let o1 = order("BTC", Side::Short, dec!(0.01));
        let f1 = simulate_fill(&o1, dec!(60000), FillIntent::Open, Decimal::ZERO);
        let o2 = order("ETH", Side::Long, dec!(1));
        let f2 = simulate_fill(&o2, dec!(3000), FillIntent::Open, Decimal::ZERO);

        append(&path, &f1).unwrap();
        append(&path, &f2).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed: Vec<Fill> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(parsed[0].asset, "BTC");
        assert_eq!(parsed[1].asset, "ETH");
        assert_eq!(parsed[0].intent, FillIntent::Open);
    }
}
