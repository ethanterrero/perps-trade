//! Order placement, fill tracking, and notional-balance rebalancing.
//!
//! Two modes:
//!   - DryRun: simulate fills at venue mid, no orders sent (Phase 2 paper trading).
//!   - Live:   send signed orders to the venue (Phase 4+).
//!
//! Phase 2 lives entirely in the DryRun path: [`simulate_fill`] produces a [`Fill`]
//! at the supplied mid price with zero fees and zero slippage. Real-world fees and
//! slippage modelling will land in a later iteration once we have data to calibrate.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use perps_types::{Order, Side, Venue};
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
    /// Total fees paid in USD. Zero in dry-run for now.
    pub fees_usd: Decimal,
    pub filled_at: DateTime<Utc>,
    pub intent: FillIntent,
    /// `client_id` of the originating Order — lets us link Fills back to the
    /// decision that produced them.
    pub order_id: uuid::Uuid,
}

/// Dry-run fill simulation: use the order's limit price if specified, otherwise
/// the supplied `mid_price`. No partial fills, no slippage, no fees. The intent
/// is supplied by the caller (the strategy/bot knows whether the order is opening
/// or closing a position; the executor doesn't need to figure it out).
pub fn simulate_fill(order: &Order, mid_price: Decimal, intent: FillIntent) -> Fill {
    let price = order.limit_price.unwrap_or(mid_price);
    Fill {
        venue: order.venue,
        asset: order.asset.clone(),
        side: order.side,
        size: order.size,
        price,
        notional_usd: order.size * price,
        fees_usd: Decimal::ZERO,
        filled_at: Utc::now(),
        intent,
        order_id: order.client_id,
    }
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
        let fill = simulate_fill(&o, dec!(60000), FillIntent::Open);
        assert_eq!(fill.price, dec!(60000));
        assert_eq!(fill.size, dec!(0.01));
        assert_eq!(fill.notional_usd, dec!(600));
        assert_eq!(fill.fees_usd, Decimal::ZERO);
        assert_eq!(fill.intent, FillIntent::Open);
        assert_eq!(fill.order_id, o.client_id);
    }

    #[test]
    fn limit_order_fills_at_limit_not_mid() {
        let mut o = order("BTC", Side::Long, dec!(0.05));
        o.limit_price = Some(dec!(59500));
        let fill = simulate_fill(&o, dec!(60000), FillIntent::Open);
        assert_eq!(fill.price, dec!(59500));
        assert_eq!(fill.notional_usd, dec!(0.05) * dec!(59500));
    }

    #[test]
    fn close_intent_carries_through() {
        let o = order("ETH", Side::Long, dec!(1));
        let fill = simulate_fill(&o, dec!(3000), FillIntent::Close);
        assert_eq!(fill.intent, FillIntent::Close);
        // Side records the closing order's direction (Long to close a Short).
        assert_eq!(fill.side, Side::Long);
    }

    #[test]
    fn append_roundtrips_through_jsonl() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("fills.jsonl");

        let o1 = order("BTC", Side::Short, dec!(0.01));
        let f1 = simulate_fill(&o1, dec!(60000), FillIntent::Open);
        let o2 = order("ETH", Side::Long, dec!(1));
        let f2 = simulate_fill(&o2, dec!(3000), FillIntent::Open);

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
