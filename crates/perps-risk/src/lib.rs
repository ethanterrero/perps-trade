//! Pre-trade and continuous risk checks.
//!
//! Phase 2 surface: compute notional, simulated margin usage, and would-be
//! liquidation prices for paper-trade positions. Enforcement (refuse-open
//! gates, kill-switch handler) lands in Phase 3 when we wire it to the
//! executor's "should we send this order" decision.
//!
//! At leverage 1 the liq prices computed here are at the extreme tails —
//! ~2× entry for Shorts, ~0.5% of entry for Longs given a 0.5% maintenance
//! margin ratio — i.e. practically unreachable. The math is correct now so
//! the same code serves Phase 4 mainnet at higher leverage without rework.

use std::collections::HashMap;

use perps_types::{Position, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Hyperliquid majors (BTC/ETH/SOL) use ~0.5% maintenance margin. Smaller
/// alts use higher ratios (1–3%). Single conservative default for now;
/// per-asset overrides can be added when we trade non-majors.
pub const DEFAULT_MAINTENANCE_MARGIN_RATIO: Decimal = dec!(0.005);

/// Notional in USD: `size * mark_price`. Mark is the right price for
/// portfolio valuation since it's what the venue uses for PnL marking.
pub fn notional_usd(position: &Position, mark_price: Decimal) -> Decimal {
    position.size * mark_price
}

/// Simulated margin used (USD) under cross-margin: notional / leverage.
/// Defensive on zero / negative leverage — treat as 1x rather than panic.
pub fn margin_used(position: &Position, mark_price: Decimal) -> Decimal {
    let n = notional_usd(position, mark_price);
    if position.leverage > Decimal::ZERO {
        n / position.leverage
    } else {
        n
    }
}

/// Would-be liquidation price (USD per base unit).
///
/// Standard isolated-margin formula:
///   Long  → entry * (1 - (1 - mmr) / leverage)
///   Short → entry * (1 + (1 - mmr) / leverage)
///
/// Leverage below 1 is clamped to 1 (sub-1x doesn't make sense and is
/// usually a config error).
pub fn liquidation_price(position: &Position, mmr: Decimal) -> Decimal {
    let entry = position.entry_price;
    let leverage = position.leverage.max(Decimal::ONE);
    let buffer = (Decimal::ONE - mmr) / leverage;
    match position.side {
        Side::Long => entry * (Decimal::ONE - buffer),
        Side::Short => entry * (Decimal::ONE + buffer),
    }
}

/// Distance from current mark to the liquidation price, as a fraction of
/// the mark. Positive = safe. Negative = position is already past
/// liquidation (shouldn't happen on a live venue but can with stale data).
pub fn liquidation_buffer_pct(position: &Position, mark_price: Decimal, mmr: Decimal) -> Decimal {
    if mark_price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let liq = liquidation_price(position, mmr);
    match position.side {
        Side::Long => (mark_price - liq) / mark_price,
        Side::Short => (liq - mark_price) / mark_price,
    }
}

/// Total notional across all positions, valued at the supplied marks. Falls
/// back to `entry_price` for any asset missing from `mark_prices`.
pub fn portfolio_notional(positions: &[Position], mark_prices: &HashMap<String, Decimal>) -> Decimal {
    positions
        .iter()
        .map(|p| {
            let mark = mark_prices
                .get(&p.asset.to_ascii_uppercase())
                .copied()
                .unwrap_or(p.entry_price);
            notional_usd(p, mark)
        })
        .sum()
}

/// Total simulated margin used across all positions.
pub fn portfolio_margin(positions: &[Position], mark_prices: &HashMap<String, Decimal>) -> Decimal {
    positions
        .iter()
        .map(|p| {
            let mark = mark_prices
                .get(&p.asset.to_ascii_uppercase())
                .copied()
                .unwrap_or(p.entry_price);
            margin_used(p, mark)
        })
        .sum()
}

/// Net directional exposure across all positions, in USD. Sum of each
/// position's signed notional (+ long, − short), valued at the supplied marks
/// with a fallback to `entry_price` for any asset missing from `mark_prices`.
///
/// This is the headline number for a delta-neutral book: it should sit near
/// zero. A single-leg perp book (no spot hedge) reports its full perp notional
/// here — i.e. the strategy is currently *directional*, not delta-neutral, and
/// this surfaces exactly how far off zero we are.
pub fn net_delta_usd(positions: &[Position], mark_prices: &HashMap<String, Decimal>) -> Decimal {
    positions
        .iter()
        .map(|p| {
            let mark = mark_prices
                .get(&p.asset.to_ascii_uppercase())
                .copied()
                .unwrap_or(p.entry_price);
            p.signed_notional(mark)
        })
        .sum()
}

/// The spot hedge leg that neutralizes a perp `position`: opposite side, equal
/// notional (hence equal base-unit size at the same mark), 1x. Pairing the perp
/// with this position drives `net_delta_usd` to zero.
///
/// Pure compute — it returns the *intended* hedge as a [`Position`], not an
/// order. The executor turns it into a spot order when the spot leg is wired
/// (see the delta-neutral execution plan). `margin_used` is set to the hedge
/// notional (spot is unleveraged); `leverage` is 1.
pub fn hedge_position_for(position: &Position, mark_price: Decimal) -> Position {
    let notional = notional_usd(position, mark_price);
    Position {
        venue: position.venue,
        asset: position.asset.clone(),
        side: position.side.flip(),
        size: position.size,
        entry_price: mark_price,
        leverage: Decimal::ONE,
        margin_used: notional,
        liquidation_price: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use perps_types::Venue;
    use rust_decimal_macros::dec;

    fn position(side: Side, entry: Decimal, size: Decimal, leverage: Decimal) -> Position {
        // margin_used isn't read by these tests, so a zero placeholder keeps
        // construction simple when leverage is 0 in the "treats zero as 1x" case.
        Position {
            venue: Venue::Hyperliquid,
            asset: "BTC".into(),
            side,
            size,
            entry_price: entry,
            leverage,
            margin_used: Decimal::ZERO,
            liquidation_price: None,
        }
    }

    #[test]
    fn notional_is_size_times_mark() {
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        assert_eq!(notional_usd(&p, dec!(61000)), dec!(30500)); // 0.5 * 61000
    }

    #[test]
    fn margin_at_1x_equals_notional() {
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        assert_eq!(margin_used(&p, dec!(60000)), dec!(30000));
    }

    #[test]
    fn margin_scales_inversely_with_leverage() {
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(5));
        // notional 30000, leverage 5 → margin 6000.
        assert_eq!(margin_used(&p, dec!(60000)), dec!(6000));
    }

    #[test]
    fn margin_treats_zero_leverage_as_1x() {
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(0));
        assert_eq!(margin_used(&p, dec!(60000)), dec!(30000));
    }

    #[test]
    fn liq_price_short_with_1x_is_about_2x_entry() {
        // Short, entry 60000, leverage 1, mmr 0.005:
        // liq = 60000 * (1 + (1 - 0.005)/1) = 60000 * 1.995 = 119700.
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        assert_eq!(
            liquidation_price(&p, DEFAULT_MAINTENANCE_MARGIN_RATIO),
            dec!(119700)
        );
    }

    #[test]
    fn liq_price_long_with_1x_is_near_zero() {
        // Long, entry 60000, leverage 1, mmr 0.005:
        // liq = 60000 * (1 - (1 - 0.005)/1) = 60000 * 0.005 = 300.
        let p = position(Side::Long, dec!(60000), dec!(0.5), dec!(1));
        assert_eq!(
            liquidation_price(&p, DEFAULT_MAINTENANCE_MARGIN_RATIO),
            dec!(300)
        );
    }

    #[test]
    fn liq_price_short_at_5x_is_about_1_2x_entry() {
        // Short, entry 60000, leverage 5, mmr 0.005:
        // liq = 60000 * (1 + 0.995/5) = 60000 * 1.199 = 71940.
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(5));
        assert_eq!(
            liquidation_price(&p, DEFAULT_MAINTENANCE_MARGIN_RATIO),
            dec!(71940)
        );
    }

    #[test]
    fn liq_buffer_short_safe_at_lower_mark() {
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(5));
        let liq = liquidation_price(&p, DEFAULT_MAINTENANCE_MARGIN_RATIO); // 71940
        // Mark at entry → buffer = (liq - mark) / mark = 11940 / 60000 = 0.199
        let buf = liquidation_buffer_pct(&p, dec!(60000), DEFAULT_MAINTENANCE_MARGIN_RATIO);
        assert!(buf > dec!(0.19) && buf < dec!(0.20), "buffer was {buf}");
        // Mark above liq means already liquidated → negative.
        let bad = liquidation_buffer_pct(&p, liq + dec!(1000), DEFAULT_MAINTENANCE_MARGIN_RATIO);
        assert!(bad.is_sign_negative());
    }

    #[test]
    fn liq_buffer_long_safe_at_higher_mark() {
        let p = position(Side::Long, dec!(60000), dec!(0.5), dec!(5));
        let buf = liquidation_buffer_pct(&p, dec!(60000), DEFAULT_MAINTENANCE_MARGIN_RATIO);
        // liq = 60000 * (1 - 0.995/5) = 60000 * 0.801 = 48060.
        // buffer = (60000 - 48060)/60000 = 0.199.
        assert!(buf > dec!(0.19) && buf < dec!(0.20));
    }

    #[test]
    fn portfolio_notional_sums_across_positions_with_mark_lookup() {
        let mut p_btc = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        p_btc.asset = "BTC".into();
        let mut p_eth = position(Side::Short, dec!(3000), dec!(2), dec!(1));
        p_eth.asset = "ETH".into();

        let mut marks = HashMap::new();
        marks.insert("BTC".into(), dec!(61000));
        marks.insert("ETH".into(), dec!(3100));

        let total = portfolio_notional(&[p_btc, p_eth], &marks);
        // 0.5 * 61000 + 2 * 3100 = 30500 + 6200 = 36700
        assert_eq!(total, dec!(36700));
    }

    #[test]
    fn portfolio_notional_falls_back_to_entry_when_mark_missing() {
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        let marks = HashMap::new();
        let total = portfolio_notional(&[p], &marks);
        assert_eq!(total, dec!(30000)); // 0.5 * 60000 entry
    }

    #[test]
    fn net_delta_of_lone_short_is_full_negative_notional() {
        // A single-leg perp book is NOT delta-neutral: its net delta equals the
        // perp's signed notional.
        let p = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        let marks = HashMap::new(); // falls back to entry
        assert_eq!(net_delta_usd(&[p], &marks), dec!(-30000));
    }

    #[test]
    fn net_delta_of_hedged_pair_is_zero() {
        // Short perp + its spot hedge nets to zero delta at the same mark.
        let perp = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        let hedge = hedge_position_for(&perp, dec!(60000));
        assert_eq!(hedge.side, Side::Long);
        assert_eq!(hedge.size, dec!(0.5));
        let marks = HashMap::new();
        assert_eq!(net_delta_usd(&[perp, hedge], &marks), Decimal::ZERO);
    }

    #[test]
    fn net_delta_uses_mark_for_valuation() {
        // After a price move the hedge built at entry no longer perfectly
        // neutralizes — residual delta is the drift the rebalancer must chase.
        let perp = position(Side::Short, dec!(60000), dec!(0.5), dec!(1));
        let hedge = hedge_position_for(&perp, dec!(60000));
        let mut marks = HashMap::new();
        marks.insert("BTC".to_string(), dec!(66000)); // +10%
        // Short delta = -0.5*66000 = -33000; Long hedge delta = +0.5*66000 = +33000.
        // Equal base sizes still net to zero at a common mark — drift comes from
        // unequal sizes, which the rebalancer introduces. Sanity-check the common-mark case.
        assert_eq!(net_delta_usd(&[perp, hedge], &marks), Decimal::ZERO);
    }
}
