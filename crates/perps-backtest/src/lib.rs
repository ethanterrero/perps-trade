//! Historical funding-rate replay for the hedged carry strategy.
//!
//! Replays a funding history through the same [`FundingWindow`] + [`decide`]
//! the bot runs live, and charges taker fees and slippage on both legs of every
//! entry and exit. That's the whole point of this crate: the dry-run executor
//! used to fill at mid for free, which hid that the raw-hourly signal paid
//! several times more in fees than it collected.
//!
//! Modeling choices:
//!   - Price PnL is zero. The perp short and spot long are the same base size,
//!     so price moves cancel. Perp/spot basis at entry and exit is not modeled.
//!   - Each history point is one funding payment of `rate * notional`, credited
//!     if a position was open going into it. Positive rate = the short perp earns.
//!   - An entry or exit costs `notional * (perp_taker + spot_taker + 2 * slippage)`.
//!   - A position still open at the end is charged its exit cost, so results
//!     never look better for having stopped mid-trade.
//!   - Returns are on notional per leg. Capital needed is roughly the spot leg
//!     plus perp margin, so return on capital is lower unless the perp is levered.

use chrono::{DateTime, Utc};
use perps_strategy::{decide, Decision, FundingSignal, FundingWindow, PortfolioState, Thresholds};
use perps_types::{FeeSchedule, FundingRate, Instrument, Position, Side};
use rust_decimal::Decimal;
use serde::Serialize;

/// Hours in a (non-leap) year, for annualizing.
const HOURS_PER_YEAR: i64 = 24 * 365;

#[derive(Debug, Clone, Copy)]
pub struct BacktestParams {
    pub thresholds: Thresholds,
    /// Trailing window for the funding signal (168 = 7 days).
    pub window_hours: u32,
    /// Hours of history required before the signal is trusted.
    pub min_signal_hours: u32,
    pub fees: FeeSchedule,
    /// Slippage per leg per fill, as a fraction of notional (0.0002 = 2 bps).
    pub slippage: Decimal,
    /// Notional per leg in USD.
    pub notional_usd: Decimal,
}

impl BacktestParams {
    /// Same params, but enter as soon as the signal exists and never exit —
    /// the passive "always carry" baseline to compare a threshold rule against.
    pub fn always_on(self) -> Self {
        Self {
            thresholds: Thresholds {
                min_apy_to_enter: Decimal::MIN,
                max_apy_to_exit: Decimal::MIN,
            },
            ..self
        }
    }

    /// One-way cost of entering or exiting both legs.
    fn round_cost(&self) -> (Decimal, Decimal) {
        let fees = self.notional_usd
            * (self.fees.taker_rate(Instrument::Perp) + self.fees.taker_rate(Instrument::Spot));
        let slippage = self.notional_usd * self.slippage * Decimal::TWO;
        (fees, slippage)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestReport {
    pub asset: String,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
    /// Funding payments replayed.
    pub hours: usize,
    pub hours_in_position: usize,
    /// Number of times a position was opened.
    pub entries: usize,
    pub funding_usd: Decimal,
    pub fees_usd: Decimal,
    pub slippage_usd: Decimal,
    pub open_at_end: bool,
    pub notional_usd: Decimal,
}

impl BacktestReport {
    pub fn net_usd(&self) -> Decimal {
        self.funding_usd - self.fees_usd - self.slippage_usd
    }

    /// Net return per year on notional, or zero for an empty history.
    pub fn net_apy(&self) -> Decimal {
        if self.hours == 0 || self.notional_usd.is_zero() {
            return Decimal::ZERO;
        }
        let years = Decimal::from(self.hours as i64) / Decimal::from(HOURS_PER_YEAR);
        self.net_usd() / self.notional_usd / years
    }

    pub fn time_in_position(&self) -> Decimal {
        if self.hours == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(self.hours_in_position as i64) / Decimal::from(self.hours as i64)
    }
}

/// Replay `history` (any order; sorted here) for `asset`.
pub fn run(asset: &str, history: &[FundingRate], params: &BacktestParams) -> BacktestReport {
    let mut points: Vec<&FundingRate> = history.iter().collect();
    points.sort_by_key(|p| p.observed_at);

    let mut window = FundingWindow::new(params.window_hours, params.min_signal_hours);
    let mut portfolio = PortfolioState::default();
    let (leg_fees, leg_slippage) = params.round_cost();
    let mut report = BacktestReport {
        asset: asset.to_string(),
        start: points.first().map(|p| p.observed_at),
        end: points.last().map(|p| p.observed_at),
        hours: points.len(),
        hours_in_position: 0,
        entries: 0,
        funding_usd: Decimal::ZERO,
        fees_usd: Decimal::ZERO,
        slippage_usd: Decimal::ZERO,
        open_at_end: false,
        notional_usd: params.notional_usd,
    };

    for point in points {
        // This payment settles the interval we were already holding through.
        if portfolio.position_for(asset).is_some() {
            report.funding_usd += point.rate * params.notional_usd;
            report.hours_in_position += 1;
        }

        window.record(point.observed_at, point.annualized());
        let Some(apy) = window.mean() else { continue };
        let signal = FundingSignal {
            venue: point.venue,
            asset: asset.to_string(),
            apy,
        };
        match decide(&portfolio, &signal, &params.thresholds, params.notional_usd) {
            Decision::Open { .. } => {
                portfolio.open(Position {
                    venue: point.venue,
                    asset: asset.to_string(),
                    instrument: Instrument::Perp,
                    side: Side::Short,
                    size: Decimal::ONE,
                    entry_price: Decimal::ONE,
                    leverage: Decimal::ONE,
                    margin_used: params.notional_usd,
                    liquidation_price: None,
                });
                report.entries += 1;
                report.fees_usd += leg_fees;
                report.slippage_usd += leg_slippage;
            }
            Decision::Close { .. } => {
                portfolio.close(asset);
                report.fees_usd += leg_fees;
                report.slippage_usd += leg_slippage;
            }
            Decision::Hold { .. } => {}
        }
    }

    if portfolio.position_for(asset).is_some() {
        report.open_at_end = true;
        report.fees_usd += leg_fees;
        report.slippage_usd += leg_slippage;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use perps_types::Venue;
    use rust_decimal_macros::dec;

    /// Hourly rate that annualizes to `apy` (rate * 8760).
    fn hourly(apy: Decimal) -> Decimal {
        apy / Decimal::from(HOURS_PER_YEAR)
    }

    fn series(apys: &[Decimal]) -> Vec<FundingRate> {
        apys.iter()
            .enumerate()
            .map(|(i, apy)| FundingRate {
                venue: Venue::Hyperliquid,
                asset: "BTC".into(),
                rate: hourly(*apy),
                interval_hours: 1,
                observed_at: Utc.timestamp_opt(i as i64 * 3600, 0).unwrap(),
            })
            .collect()
    }

    fn params(window_hours: u32) -> BacktestParams {
        BacktestParams {
            thresholds: Thresholds {
                min_apy_to_enter: dec!(0.10),
                max_apy_to_exit: dec!(0.02),
            },
            window_hours,
            min_signal_hours: window_hours,
            fees: FeeSchedule::hyperliquid_base(),
            slippage: dec!(0.0001),
            notional_usd: dec!(1000),
        }
    }

    #[test]
    fn never_trades_below_entry_threshold() {
        let r = run("BTC", &series(&[dec!(0.05); 100]), &params(24));
        assert_eq!(r.entries, 0);
        assert_eq!(r.net_usd(), Decimal::ZERO);
    }

    #[test]
    fn charges_both_legs_on_entry_and_end_of_data_exit() {
        // Window of 1 → signal exists from the first hour, opens there, earns
        // the next 9 payments, then is charged its exit at end of data.
        let r = run("BTC", &series(&[dec!(0.876); 10]), &params(1));
        assert_eq!(r.entries, 1);
        assert_eq!(r.hours_in_position, 9);
        assert!(r.open_at_end);
        // 9 payments of 0.0001 * 1000.
        assert_eq!(r.funding_usd, dec!(0.9));
        // (0.045% + 0.07%) * 1000 per side, twice.
        assert_eq!(r.fees_usd, dec!(2.30));
        // 1 bp * 2 legs * 1000 per side, twice.
        assert_eq!(r.slippage_usd, dec!(0.40));
        assert_eq!(r.net_usd(), dec!(-1.80));
    }

    #[test]
    fn closes_when_funding_decays_and_charges_exit_once() {
        let mut apys = vec![dec!(0.20); 5];
        apys.extend(vec![dec!(0.00); 5]);
        let r = run("BTC", &series(&apys), &params(1));
        assert_eq!(r.entries, 1);
        assert!(!r.open_at_end);
        assert_eq!(r.hours_in_position, 5); // hours 1..=5; closes at hour 5
        assert_eq!(r.fees_usd, dec!(2.30));
    }

    #[test]
    fn smoothing_cuts_churn_on_choppy_funding() {
        // Funding alternating 0% / 24% every hour averages 12%, above the
        // entry line. The raw signal flips in and out every two hours; the
        // 24h mean sits steady and enters once.
        let apys: Vec<Decimal> = (0..500)
            .map(|i| if i % 2 == 0 { dec!(0.0) } else { dec!(0.24) })
            .collect();
        let raw = run("BTC", &series(&apys), &params(1));
        let smooth = run("BTC", &series(&apys), &params(24));
        assert!(raw.entries > 100, "raw entries {}", raw.entries);
        assert_eq!(smooth.entries, 1);
        assert!(smooth.net_usd() > raw.net_usd());
    }

    #[test]
    fn always_on_baseline_holds_through_negative_funding() {
        let mut apys = vec![dec!(0.20); 5];
        apys.extend(vec![dec!(-0.20); 5]);
        let r = run("BTC", &series(&apys), &params(1).always_on());
        assert_eq!(r.entries, 1);
        assert!(r.open_at_end);
        assert_eq!(r.hours_in_position, 9);
    }

    #[test]
    fn net_apy_annualizes_over_replayed_hours() {
        // Always in position at a constant 8.76% APY for a full year, no costs.
        let mut p = params(1).always_on();
        p.fees = FeeSchedule {
            perp_taker: Decimal::ZERO,
            spot_taker: Decimal::ZERO,
        };
        p.slippage = Decimal::ZERO;
        let r = run("BTC", &series(&vec![dec!(0.0876); 8761]), &p);
        assert_eq!(r.hours_in_position, 8760);
        assert_eq!(r.funding_usd, dec!(87.6));
        // 8761 replayed hours ≈ one year; within a rounding hair of 8.76%.
        assert!((r.net_apy() - dec!(0.0876)).abs() < dec!(0.0001));
    }

    #[test]
    fn empty_history_is_a_zero_report() {
        let r = run("BTC", &[], &params(24));
        assert_eq!(r.hours, 0);
        assert_eq!(r.net_apy(), Decimal::ZERO);
        assert!(r.start.is_none());
    }
}
