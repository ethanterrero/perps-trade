//! `perps-bot digest`: format the per-asset PnL summary derived from `pnl.rs`.
//! Pure presentation — the math lives in `pnl::`.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;
use perps_executor::FILLS_LOG_FILE;
use perps_risk::{liquidation_buffer_pct, liquidation_price, DEFAULT_MAINTENANCE_MARGIN_RATIO};
use perps_types::Position;
use rust_decimal::Decimal;

use crate::pnl;

#[derive(Default, Debug)]
struct AssetPnl {
    closed_count: usize,
    open_count: usize,
    realized: Decimal,
    unrealized: Decimal,
    funding: Decimal,
}

pub fn run(state_dir: &Path) -> anyhow::Result<()> {
    let fills = pnl::read_fills(state_dir)?;
    let observations = pnl::read_funding(state_dir)?;

    if fills.is_empty() {
        println!(
            "no fills recorded yet ({})",
            state_dir.join(FILLS_LOG_FILE).display()
        );
        return Ok(());
    }

    let (closed, open) = pnl::pair_fills(&fills);
    let mids = pnl::latest_mids(&observations);

    let mut rows: BTreeMap<String, AssetPnl> = BTreeMap::new();
    let now = Utc::now();
    // For still-open positions, funding accrual ends at the most recent
    // observation rather than `now`. Wall-clock time after we stopped
    // polling isn't observable — pretending it accrues at the last seen
    // rate overstates funding when the digest runs hours after the bot.
    let last_observed = observations
        .last()
        .map(|o| o.observed_at)
        .unwrap_or(now);

    for trade in &closed {
        let funding = pnl::accrued_funding(
            &trade.asset,
            trade.side,
            trade.open.notional_usd,
            trade.open.filled_at,
            trade.close.filled_at,
            &observations,
        );
        let row = rows
            .entry(trade.asset.to_ascii_uppercase())
            .or_default();
        row.closed_count += 1;
        row.realized += trade.realized_pnl();
        row.funding += funding;
    }

    for trade in &open {
        let key = trade.asset.to_ascii_uppercase();
        let mid = mids.get(&key).copied().unwrap_or(trade.open.price);
        let unrealized = trade.unrealized_pnl(mid);
        let funding = pnl::accrued_funding(
            &trade.asset,
            trade.side,
            trade.open.notional_usd,
            trade.open.filled_at,
            last_observed,
            &observations,
        );
        let row = rows.entry(key).or_default();
        row.open_count += 1;
        row.unrealized += unrealized;
        row.funding += funding;
    }

    // Period spans both fills and observations so a "held the whole run" digest
    // shows the actual run duration rather than a single tick's worth.
    let earliest = [
        fills.first().map(|f| f.filled_at),
        observations.first().map(|o| o.observed_at),
    ]
    .into_iter()
    .flatten()
    .min()
    .unwrap_or(now);
    let latest = [
        fills.last().map(|f| f.filled_at),
        observations.last().map(|o| o.observed_at),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(now);
    let duration = latest - earliest;

    println!("paper-trade digest from {}", state_dir.display());
    println!();
    println!(
        "period: {} → {} ({})",
        earliest.format("%Y-%m-%dT%H:%M:%SZ"),
        latest.format("%Y-%m-%dT%H:%M:%SZ"),
        format_duration(duration),
    );
    println!(
        "fills: {}  observations: {}  open positions: {}  closed trades: {}",
        fills.len(),
        observations.len(),
        open.len(),
        closed.len(),
    );
    println!();
    println!(
        "{:<6} {:>7} {:>5} {:>12} {:>12} {:>12} {:>12}",
        "asset", "trades", "open", "realized", "unrealized", "funding", "total"
    );

    let mut totals = AssetPnl::default();
    for (asset, row) in &rows {
        let total = row.realized + row.unrealized + row.funding;
        totals.realized += row.realized;
        totals.unrealized += row.unrealized;
        totals.funding += row.funding;
        totals.closed_count += row.closed_count;
        totals.open_count += row.open_count;
        println!(
            "{:<6} {:>7} {:>5} {:>12} {:>12} {:>12} {:>12}",
            asset,
            row.closed_count,
            row.open_count,
            format_usd(row.realized),
            format_usd(row.unrealized),
            format_usd(row.funding),
            format_usd(total),
        );
    }

    let grand_total = totals.realized + totals.unrealized + totals.funding;
    println!();
    println!(
        "totals: realized {}  unrealized {}  funding {}  net {}",
        format_usd(totals.realized),
        format_usd(totals.unrealized),
        format_usd(totals.funding),
        format_usd(grand_total),
    );

    if !open.is_empty() {
        println!();
        println!("open positions:");
        println!(
            "{:<6} {:<6} {:>14} {:>12} {:>12} {:>14} {:>10} {:>14}",
            "asset", "side", "size", "entry", "mark", "liq_price", "buffer", "delta_usd"
        );
        let mut net_delta = Decimal::ZERO;
        for trade in &open {
            let key = trade.asset.to_ascii_uppercase();
            let mark = mids.get(&key).copied().unwrap_or(trade.open.price);
            let position = Position {
                venue: trade.open.venue,
                asset: trade.asset.clone(),
                side: trade.side,
                size: trade.open.size,
                entry_price: trade.open.price,
                leverage: Decimal::ONE,
                margin_used: trade.open.notional_usd,
                liquidation_price: None,
            };
            let liq = liquidation_price(&position, DEFAULT_MAINTENANCE_MARGIN_RATIO);
            let buf = liquidation_buffer_pct(&position, mark, DEFAULT_MAINTENANCE_MARGIN_RATIO);
            let delta = position.signed_notional(mark);
            net_delta += delta;
            let side = match trade.side {
                perps_types::Side::Long => "long",
                perps_types::Side::Short => "short",
            };
            println!(
                "{:<6} {:<6} {:>14} {:>12} {:>12} {:>14} {:>10} {:>14}",
                trade.asset,
                side,
                format_decimal(position.size, 6),
                format_decimal(position.entry_price, 2),
                format_decimal(mark, 2),
                format_decimal(liq, 2),
                format_pct(buf),
                format_usd(delta),
            );
        }

        // Gross long exposure for context: |net delta| / gross tells you how
        // hedged the book is. Gross is the sum of |signed_notional|.
        let gross: Decimal = open
            .iter()
            .map(|t| {
                let key = t.asset.to_ascii_uppercase();
                let mark = mids.get(&key).copied().unwrap_or(t.open.price);
                (t.open.size * mark).abs()
            })
            .sum();
        println!();
        println!("net delta: {} (gross {})", format_usd(net_delta), format_usd(gross));
        if gross > Decimal::ZERO && net_delta.abs() * Decimal::from(100) >= gross {
            // |net delta| >= 1% of gross — the book carries material directional
            // exposure. With no spot hedge wired, a single-leg perp book sits at
            // 100% here. See docs/reports for the path to a real two-leg hedge.
            println!(
                "  ⚠ book is directional, not delta-neutral — hedge leg not yet wired (single-leg perp)"
            );
        }
    }

    Ok(())
}

fn format_decimal(d: Decimal, dp: u32) -> String {
    format!("{}", d.round_dp(dp).normalize())
}

fn format_pct(d: Decimal) -> String {
    let pct = (d * Decimal::from(100)).round_dp(1);
    format!("{pct}%")
}

fn format_usd(d: Decimal) -> String {
    // Round to four decimal places for compact display. Use a leading sign so
    // negative numbers are unambiguous.
    let rounded = d.round_dp(4);
    if rounded.is_sign_negative() {
        format!("-${}", (-rounded).normalize())
    } else {
        format!("${}", rounded.normalize())
    }
}

fn format_duration(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}
