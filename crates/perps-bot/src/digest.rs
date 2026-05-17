//! `perps-bot digest`: format the per-asset PnL summary derived from `pnl.rs`.
//! Pure presentation — the math lives in `pnl::`.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::Utc;
use perps_executor::FILLS_LOG_FILE;
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
            now,
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

    Ok(())
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
