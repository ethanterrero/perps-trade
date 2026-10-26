//! Hedged funding-carry entry/exit decision logic.
//!
//! The trade: short the perp and buy the same base size on spot. Price moves
//! cancel; the short perp collects funding while funding is positive. Decisions
//! are pure functions of (portfolio snapshot, signal, thresholds, sizing) — no
//! I/O, no async.
//!
//! The signal is a trailing mean of annualized funding ([`FundingWindow`]), not
//! the latest hourly print. Backtests against a year of Hyperliquid mainnet
//! funding showed raw hourly thresholds re-entering 200+ times a year, paying
//! several times more in fees than the funding they collected; a 7-day mean
//! trades a handful of times a year.
//!
//! Carry only runs one way. Positive funding (longs pay shorts) is harvested by
//! short perp + long spot. Negative funding would need short spot, which the
//! venue doesn't offer, so negative funding is a reason to stay flat — never a
//! reason to open an unhedged long.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use perps_types::{Instrument, Position, Venue};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Action the executor should take for a given asset after evaluating its funding signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    /// Enter a hedged carry position in `asset`: short `notional_usd` of the
    /// perp and buy the same base size on spot.
    Open {
        asset: String,
        notional_usd: Decimal,
    },
    /// Exit every leg we hold in `asset`.
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

/// Trailing mean of annualized funding over `window_hours`, one value per hour.
///
/// The bot polls every few minutes but Hyperliquid funding settles hourly, so
/// each hour keeps only its latest observation. That way a 5-minute live poll
/// and an hourly history backfill are weighted the same.
#[derive(Debug, Clone)]
pub struct FundingWindow {
    window_hours: i64,
    min_hours: usize,
    buckets: BTreeMap<i64, Decimal>,
}

impl FundingWindow {
    /// `min_hours` is how many distinct hours must be present before [`mean`]
    /// returns a value; clamped to `window_hours`.
    ///
    /// [`mean`]: FundingWindow::mean
    pub fn new(window_hours: u32, min_hours: u32) -> Self {
        let window_hours = window_hours.max(1);
        Self {
            window_hours: i64::from(window_hours),
            min_hours: min_hours.min(window_hours) as usize,
            buckets: BTreeMap::new(),
        }
    }

    /// Record an annualized observation. Later observations in the same hour
    /// replace earlier ones; hours older than the window (measured from the
    /// newest hour seen) are dropped.
    pub fn record(&mut self, at: DateTime<Utc>, apy: Decimal) {
        let hour = at.timestamp().div_euclid(3600);
        self.buckets.insert(hour, apy);
        let newest = *self.buckets.keys().next_back().expect("just inserted");
        self.buckets = self.buckets.split_off(&(newest - self.window_hours + 1));
    }

    /// Distinct hours currently in the window.
    pub fn hours(&self) -> usize {
        self.buckets.len()
    }

    /// Mean annualized funding across the window, or `None` while fewer than
    /// `min_hours` hours have been recorded.
    pub fn mean(&self) -> Option<Decimal> {
        if self.buckets.is_empty() || self.buckets.len() < self.min_hours {
            return None;
        }
        let sum: Decimal = self.buckets.values().copied().sum();
        Some(sum / Decimal::from(self.buckets.len()))
    }
}

/// Snapshot of currently-held positions, scoped to the strategy's view. A
/// hedged asset holds two legs (perp + spot); a pre-hedge log may hold one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortfolioState {
    pub positions: Vec<Position>,
}

impl PortfolioState {
    /// Case-insensitive lookup for any open leg in `asset`.
    pub fn position_for(&self, asset: &str) -> Option<&Position> {
        self.positions
            .iter()
            .find(|p| p.asset.eq_ignore_ascii_case(asset))
    }

    /// Add a leg, replacing any existing leg for the same asset and instrument.
    pub fn open(&mut self, position: Position) {
        self.positions.retain(|p| {
            !(p.asset.eq_ignore_ascii_case(&position.asset) && p.instrument == position.instrument)
        });
        self.positions.push(position);
    }

    /// Remove and return every leg for `asset` (case-insensitive), perp first.
    pub fn close(&mut self, asset: &str) -> Vec<Position> {
        let (mut closed, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.positions)
            .into_iter()
            .partition(|p| p.asset.eq_ignore_ascii_case(asset));
        self.positions = kept;
        closed.sort_by_key(|p| p.instrument != Instrument::Perp);
        closed
    }
}

/// Threshold parameters for the carry entry/exit rule. The caller maps these
/// from `perps_config::StrategySettings` (or wherever they live) at the call
/// site — keeping `perps-strategy` free of config plumbing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    /// Smoothed APY at or above this triggers an `Open` when not in position.
    pub min_apy_to_enter: Decimal,
    /// Smoothed APY strictly below this triggers a `Close` when in position.
    pub max_apy_to_exit: Decimal,
}

/// Carry rule on the smoothed signal:
///
/// * In position → `Close` if `apy < max_apy_to_exit` (including any negative
///   APY, where the short perp would be paying), else `Hold`.
/// * Not in position → `Open` if `apy >= min_apy_to_enter`, else `Hold`.
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
        if signal.apy < thresholds.max_apy_to_exit {
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
    use chrono::TimeZone;
    use perps_types::Side;
    use rust_decimal_macros::dec;

    fn leg(asset: &str, instrument: Instrument, side: Side) -> Position {
        Position {
            venue: Venue::Hyperliquid,
            asset: asset.into(),
            instrument,
            side,
            size: dec!(1),
            entry_price: dec!(100),
            leverage: dec!(1),
            margin_used: dec!(100),
            liquidation_price: None,
        }
    }

    fn position(asset: &str, side: Side) -> Position {
        leg(asset, Instrument::Perp, side)
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

    fn hour(h: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(h * 3600, 0).unwrap()
    }

    const NOTIONAL: Decimal = dec!(500);

    #[test]
    fn holds_when_no_position_and_below_entry_threshold() {
        let state = PortfolioState::default();
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.05)), &thresholds(), NOTIONAL),
            Decision::Hold { asset: "BTC".into() }
        );
    }

    #[test]
    fn opens_carry_on_high_positive_funding() {
        let state = PortfolioState::default();
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.15)), &thresholds(), NOTIONAL),
            Decision::Open {
                asset: "BTC".into(),
                notional_usd: NOTIONAL,
            }
        );
    }

    #[test]
    fn never_opens_on_negative_funding() {
        // Harvesting negative funding needs short spot, which we can't hedge
        // with — so no unhedged long, however negative funding gets.
        let state = PortfolioState::default();
        assert_eq!(
            decide(&state, &signal("BTC", dec!(-0.50)), &thresholds(), NOTIONAL),
            Decision::Hold { asset: "BTC".into() }
        );
    }

    #[test]
    fn enters_at_threshold_boundary() {
        // APY exactly at threshold counts as an enter (>=).
        let state = PortfolioState::default();
        assert!(matches!(
            decide(&state, &signal("BTC", dec!(0.10)), &thresholds(), NOTIONAL),
            Decision::Open { .. }
        ));
    }

    #[test]
    fn closes_when_funding_decays_below_exit() {
        let state = PortfolioState {
            positions: vec![position("BTC", Side::Short)],
        };
        assert_eq!(
            decide(&state, &signal("BTC", dec!(0.01)), &thresholds(), NOTIONAL),
            Decision::Close { asset: "BTC".into() }
        );
    }

    #[test]
    fn closes_when_funding_turns_negative() {
        // The old |apy| rule held through strongly negative funding; a short
        // perp pays then, so it must exit.
        let state = PortfolioState {
            positions: vec![position("BTC", Side::Short)],
        };
        assert_eq!(
            decide(&state, &signal("BTC", dec!(-0.30)), &thresholds(), NOTIONAL),
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
            Decision::Open { .. }
        ));
        // Same asset, different case — still finds the BTC position.
        assert_eq!(
            decide(&state, &signal("btc", dec!(0.01)), &thresholds(), NOTIONAL),
            Decision::Close { asset: "btc".into() }
        );
    }

    #[test]
    fn portfolio_open_replaces_same_leg() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        state.open(position("BTC", Side::Long)); // same perp leg: replace, not add
        assert_eq!(state.positions.len(), 1);
        assert_eq!(state.positions[0].side, Side::Long);
    }

    #[test]
    fn portfolio_holds_perp_and_spot_legs_side_by_side() {
        let mut state = PortfolioState::default();
        state.open(leg("BTC", Instrument::Perp, Side::Short));
        state.open(leg("BTC", Instrument::Spot, Side::Long));
        assert_eq!(state.positions.len(), 2);
    }

    #[test]
    fn portfolio_open_adds_distinct_assets() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        state.open(position("ETH", Side::Short));
        assert_eq!(state.positions.len(), 2);
    }

    #[test]
    fn portfolio_close_removes_every_leg_perp_first() {
        let mut state = PortfolioState::default();
        state.open(leg("BTC", Instrument::Spot, Side::Long));
        state.open(leg("BTC", Instrument::Perp, Side::Short));
        state.open(leg("ETH", Instrument::Perp, Side::Short));
        let closed = state.close("BTC");
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[0].instrument, Instrument::Perp);
        assert_eq!(closed[1].instrument, Instrument::Spot);
        assert_eq!(state.positions.len(), 1);
        // Closing again returns nothing.
        assert!(state.close("BTC").is_empty());
    }

    #[test]
    fn portfolio_close_is_case_insensitive() {
        let mut state = PortfolioState::default();
        state.open(position("BTC", Side::Short));
        assert_eq!(state.close("btc").len(), 1);
    }

    #[test]
    fn window_is_none_until_min_hours_recorded() {
        let mut w = FundingWindow::new(168, 3);
        w.record(hour(0), dec!(0.10));
        w.record(hour(1), dec!(0.20));
        assert_eq!(w.mean(), None);
        w.record(hour(2), dec!(0.30));
        assert_eq!(w.mean(), Some(dec!(0.20)));
    }

    #[test]
    fn window_keeps_latest_observation_per_hour() {
        // A 5-minute poll lands several samples in one hour; only the last counts.
        let mut w = FundingWindow::new(168, 1);
        w.record(hour(0), dec!(0.50));
        w.record(hour(0) + chrono::Duration::minutes(55), dec!(0.10));
        w.record(hour(1), dec!(0.30));
        assert_eq!(w.hours(), 2);
        assert_eq!(w.mean(), Some(dec!(0.20)));
    }

    #[test]
    fn window_drops_hours_older_than_window() {
        let mut w = FundingWindow::new(3, 1);
        for h in 0..5 {
            w.record(hour(h), Decimal::from(h));
        }
        // Keeps hours 2, 3, 4.
        assert_eq!(w.hours(), 3);
        assert_eq!(w.mean(), Some(dec!(3)));
    }

    #[test]
    fn window_accepts_out_of_order_backfill() {
        // History backfill can arrive after a live sample; old hours still
        // count if inside the window and are dropped if not.
        let mut w = FundingWindow::new(3, 1);
        w.record(hour(10), dec!(0.30));
        w.record(hour(9), dec!(0.10));
        w.record(hour(2), dec!(9.99)); // outside window relative to hour 10
        assert_eq!(w.hours(), 2);
        assert_eq!(w.mean(), Some(dec!(0.20)));
    }

    #[test]
    fn decision_serializes_to_tagged_json() {
        let d = Decision::Open {
            asset: "BTC".into(),
            notional_usd: dec!(500),
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(v["kind"], "open");
        assert_eq!(v["asset"], "BTC");
        assert_eq!(v["notional_usd"], "500");
    }

    #[test]
    fn decision_deserializes_pre_hedge_open_with_side() {
        // decisions.jsonl written before the hedge leg carries a `side` field.
        let d: Decision = serde_json::from_str(
            r#"{"kind":"open","asset":"BTC","side":"short","notional_usd":"500"}"#,
        )
        .unwrap();
        assert!(matches!(d, Decision::Open { .. }));
    }
}
