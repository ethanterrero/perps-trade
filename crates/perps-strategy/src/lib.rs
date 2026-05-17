//! Delta-neutral entry/exit decision logic.
//!
//! Phase 2: threshold rules on annualized funding. Decisions are pure functions of
//! (portfolio snapshot, signal, thresholds, sizing) — no I/O, no async. Anything
//! fancier (ML, smoothing, regime-detection) waits until we have realized PnL to
//! compare against.
//!
//! Sign convention follows Hyperliquid funding: positive APY means longs pay shorts,
//! so we collect by going short; negative APY means shorts pay longs, so we collect
//! by going long.

use perps_types::{Position, Side, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Action the executor should take for a given asset after evaluating its funding signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// Enter a position in `asset` on `side`, sized to `notional_usd`.
    Open {
        asset: String,
        side: Side,
        notional_usd: Decimal,
    },
    /// Exit our existing position in `asset`.
    Close { asset: String },
    /// Do nothing this tick (for `asset`).
    Hold { asset: String },
}

/// Per-asset funding-rate signal, pre-annualized so the strategy is agnostic to the
/// venue's funding interval (1h on Hyperliquid, 8h on Binance, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingSignal {
    pub venue: Venue,
    pub asset: String,
    /// Signed annualized funding rate. e.g. `0.10` = 10% APY, `-0.10` = -10% APY.
    pub apy: Decimal,
}

/// Snapshot of currently-held positions, scoped to the strategy's view.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortfolioState {
    pub positions: Vec<Position>,
}

impl PortfolioState {
    /// Case-insensitive lookup for an open position in `asset`.
    pub fn position_for(&self, asset: &str) -> Option<&Position> {
        self.positions
            .iter()
            .find(|p| p.asset.eq_ignore_ascii_case(asset))
    }

    /// Replace any existing position for the same asset and add the new one.
    /// Phase 2 paper-trade assumes at most one position per asset; if we ever
    /// model partial adds, this becomes additive instead.
    pub fn open(&mut self, position: Position) {
        self.positions
            .retain(|p| !p.asset.eq_ignore_ascii_case(&position.asset));
        self.positions.push(position);
    }

    /// Remove and return the position for `asset`, if any (case-insensitive).
    pub fn close(&mut self, asset: &str) -> Option<Position> {
        let idx = self
            .positions
            .iter()
            .position(|p| p.asset.eq_ignore_ascii_case(asset))?;
        Some(self.positions.remove(idx))
    }
}

/// Threshold parameters for the symmetric-funding entry/exit rule. The caller maps
/// these from `perps_config::StrategySettings` (or wherever they live) at the call
/// site — keeping `perps-strategy` free of config plumbing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    /// |APY| at or above this triggers an `Open` when not in position.
    pub min_apy_to_enter: Decimal,
    /// |APY| strictly below this triggers a `Close` when in position.
    pub max_apy_to_exit: Decimal,
}

/// Symmetric threshold rule:
///
/// * In position → `Close` if `|apy| < max_apy_to_exit`, else `Hold`.
/// * Not in position →
///     - `apy >= min_apy_to_enter` → `Open` Short (positive funding, collect by shorting).
///     - `apy <= -min_apy_to_enter` → `Open` Long (negative funding, collect by going long).
///     - Otherwise → `Hold`.
///
/// The dead zone between `max_apy_to_exit` and `min_apy_to_enter` is intentional:
/// once we're in a position we don't keep churning when funding wobbles. Once we exit
/// we wait until APY meaningfully recovers before re-entering.
pub fn decide(
    state: &PortfolioState,
    signal: &FundingSignal,
    thresholds: &Thresholds,
    max_notional_usd: Decimal,
) -> Decision {
    if state.position_for(&signal.asset).is_some() {
        if signal.apy.abs() < thresholds.max_apy_to_exit {
            return Decision::Close {
                asset: signal.asset.clone(),
            };
        }
        return Decision::Hold {
            asset: signal.asset.clone(),
        };
    }

    if signal.apy >= thresholds.min_apy_to_enter {
        return Decision::Open {
            asset: signal.asset.clone(),
            side: Side::Short,
            notional_usd: max_notional_usd,
        };
    }
    if signal.apy <= -thresholds.min_apy_to_enter {
        return Decision::Open {
            asset: signal.asset.clone(),
            side: Side::Long,
            notional_usd: max_notional_usd,
        };
    }

    Decision::Hold {
        asset: signal.asset.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn position(asset: &str, side: Side) -> Position {
        Position {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            side,
            size: dec!(1),
            entry_price: dec!(100),
            leverage: dec!(1),
            margin_used: dec!(100),
            liquidation_price: None,
        }
    }

    fn signal(asset: &str, apy: Decimal) -> FundingSignal {
        FundingSignal {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            apy,
        }
    }

    fn thresholds() -> Thresholds {
        Thresholds {
            min_apy_to_enter: dec!(0.10),
            max_apy_to_exit: dec!(0.02),
        }
    }

    const NOTIONAL: Decimal = dec!(500);

    #[test]
    fn holds_when_no_position_and_below_entry_threshold() {
        let state = PortfolioState::default();
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.05)), &thresholds(), NOTIONAL),
            Decision::Hold { asset: "BTC".into() }
        );
        assert_eq!(
            decide(&state, &signal("BTC", dec!(-0.05)), &thresholds(), NOTIONAL),
            Decision::Hold { asset: "BTC".into() }
        );
    }

    #[test]
    fn opens_short_on_high_positive_funding() {
        let state = PortfolioState::default();
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.15)), &thresholds(), NOTIONAL),
            Decision::Open {
                asset: "BTC".into(),
                side: Side::Short,
                notional_usd: NOTIONAL,
            }
        );
    }

    #[test]
    fn opens_long_on_high_negative_funding() {
        let state = PortfolioState::default();
        assert_eq!(
            decide(&state, &signal("BTC", dec!(-0.15)), &thresholds(), NOTIONAL),
            Decision::Open {
                asset: "BTC".into(),
                side: Side::Long,
                notional_usd: NOTIONAL,
            }
        );
    }

    #[test]
    fn enters_at_threshold_boundary() {
        // APY exactly at threshold counts as an enter (>=).
        let state = PortfolioState::default();
        assert!(matches!(
            decide(&state, &signal("BTC", dec!(0.10)), &thresholds(), NOTIONAL),
            Decision::Open { side: Side::Short, .. }
        ));
        assert!(matches!(
            decide(&state, &signal("BTC", dec!(-0.10)), &thresholds(), NOTIONAL),
            Decision::Open { side: Side::Long, .. }
        ));
    }

    #[test]
    fn closes_when_funding_decays_below_exit_either_sign() {
        let state = PortfolioState {
            positions: vec![position("BTC", Side::Short)],
        };
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.01)), &thresholds(), NOTIONAL),
            Decision::Close { asset: "BTC".into() }
        );
        assert_eq!(
            decide(&state, &signal("BTC", dec!(-0.01)), &thresholds(), NOTIONAL),
            Decision::Close { asset: "BTC".into() }
        );
    }

    #[test]
    fn holds_when_in_position_and_funding_still_above_exit() {
        // Funding sits in the dead zone between exit and re-enter — no churn.
        let state = PortfolioState {
            positions: vec![position("BTC", Side::Short)],
        };
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.05)), &thresholds(), NOTIONAL),
            Decision::Hold { asset: "BTC".into() }
        );
    }

    #[test]
    fn decision_is_per_asset_case_insensitive() {
        let state = PortfolioState {
            positions: vec![position("BTC", Side::Short)],
        };
        // ETH signal — no ETH position, so evaluated as if empty.
        assert!(matches!(
            decide(&state, &signal("ETH", dec!(0.20)), &thresholds(), NOTIONAL),
            Decision::Open { side: Side::Short, .. }
        ));
        // Same asset, different case — still finds the BTC position.
        assert_eq!(
            decide(&state, &signal("btc", dec!(0.01)), &thresholds(), NOTIONAL),
            Decision::Close { asset: "btc".into() }
        );
    }

    #[test]
    fn portfolio_open_replaces_existing_position() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        state.open(position("BTC", Side::Long)); // replace, not add
        assert_eq!(state.positions.len(), 1);
        assert_eq!(state.positions[0].side, Side::Long);
    }

    #[test]
    fn portfolio_open_adds_distinct_assets() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        state.open(position("ETH", Side::Long));
        assert_eq!(state.positions.len(), 2);
    }

    #[test]
    fn portfolio_close_removes_and_returns_position() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        let closed = state.close("BTC");
        assert!(closed.is_some());
        assert_eq!(closed.unwrap().side, Side::Short);
        assert!(state.positions.is_empty());
        // Closing again returns None.
        assert!(state.close("BTC").is_none());
    }

    #[test]
    fn portfolio_close_is_case_insensitive() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        assert!(state.close("btc").is_some());
    }

    #[test]
    fn decision_serializes_to_tagged_json() {
        let d = Decision::Open {
            asset: "BTC".into(),
            side: Side::Short,
            notional_usd: dec!(500),
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(v["kind"], "open");
        assert_eq!(v["asset"], "BTC");
        assert_eq!(v["side"], "short");
        assert_eq!(v["notional_usd"], "500");
    }
}
