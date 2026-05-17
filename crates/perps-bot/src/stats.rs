use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Utc};
use perps_funding::FUNDING_LOG_FILE;
use perps_types::FundingRate;
use rust_decimal::Decimal;

use crate::decisions::{decision_asset, kind_name, DecisionRecord, DECISIONS_LOG_FILE};

/// Per-asset summary derived from the JSONL log.
struct AssetStats {
    count: usize,
    last: FundingRate,
    min_apy: Decimal,
    max_apy: Decimal,
}

pub fn run(state_dir: &Path) -> anyhow::Result<()> {
    print_funding(state_dir)?;
    print_decisions(state_dir)?;
    Ok(())
}

fn print_funding(state_dir: &Path) -> anyhow::Result<()> {
    let log_path = state_dir.join(FUNDING_LOG_FILE);

    if !log_path.exists() {
        println!("no observations recorded yet ({})", log_path.display());
        return Ok(());
    }

    let file = std::fs::File::open(&log_path)?;
    let reader = BufReader::new(file);

    let mut by_asset: BTreeMap<String, AssetStats> = BTreeMap::new();
    let mut total = 0usize;
    let mut malformed = 0usize;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let fr: FundingRate = match serde_json::from_str(&line) {
            Ok(fr) => fr,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        total += 1;
        let apy = fr.annualized();
        by_asset
            .entry(fr.asset.clone())
            .and_modify(|s| {
                s.count += 1;
                if apy < s.min_apy {
                    s.min_apy = apy;
                }
                if apy > s.max_apy {
                    s.max_apy = apy;
                }
                s.last = fr.clone();
            })
            .or_insert_with(|| AssetStats {
                count: 1,
                last: fr.clone(),
                min_apy: apy,
                max_apy: apy,
            });
    }

    if by_asset.is_empty() {
        println!("no parseable observations in {}", log_path.display());
        if malformed > 0 {
            println!("({malformed} malformed lines skipped)");
        }
        return Ok(());
    }

    println!("funding observations from {}", log_path.display());
    println!(
        "total: {total} observations across {} asset(s){}",
        by_asset.len(),
        if malformed > 0 {
            format!(" ({malformed} malformed lines skipped)")
        } else {
            String::new()
        }
    );
    println!();
    println!(
        "{:<6} {:>6} {:>14} {:>14} {:>10} {:>10} {:>10}",
        "asset", "count", "last_seen", "last_rate", "last_apy", "min_apy", "max_apy"
    );

    let now = Utc::now();
    for (asset, s) in &by_asset {
        println!(
            "{:<6} {:>6} {:>14} {:>14} {:>10} {:>10} {:>10}",
            asset,
            s.count,
            format_age(now - s.last.observed_at),
            format!("{}", s.last.rate),
            format_pct(s.last.annualized()),
            format_pct(s.min_apy),
            format_pct(s.max_apy),
        );
    }

    Ok(())
}

/// Per-asset summary derived from `decisions.jsonl`.
struct DecisionSummary {
    total: usize,
    open: usize,
    close: usize,
    hold: usize,
    last_kind: &'static str,
    last_seen: DateTime<Utc>,
}

fn print_decisions(state_dir: &Path) -> anyhow::Result<()> {
    let log_path = state_dir.join(DECISIONS_LOG_FILE);
    if !log_path.exists() {
        return Ok(());
    }

    let file = std::fs::File::open(&log_path)?;
    let reader = BufReader::new(file);

    let mut by_asset: BTreeMap<String, DecisionSummary> = BTreeMap::new();
    let mut total = 0usize;
    let mut malformed = 0usize;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rec: DecisionRecord = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        total += 1;
        let kind = kind_name(&rec.decision);
        let asset = decision_asset(&rec.decision).to_string();
        by_asset
            .entry(asset)
            .and_modify(|s| {
                s.total += 1;
                match kind {
                    "open" => s.open += 1,
                    "close" => s.close += 1,
                    "hold" => s.hold += 1,
                    _ => {}
                }
                if rec.decided_at >= s.last_seen {
                    s.last_kind = kind;
                    s.last_seen = rec.decided_at;
                }
            })
            .or_insert_with(|| DecisionSummary {
                total: 1,
                open: (kind == "open") as usize,
                close: (kind == "close") as usize,
                hold: (kind == "hold") as usize,
                last_kind: kind,
                last_seen: rec.decided_at,
            });
    }

    if by_asset.is_empty() {
        return Ok(());
    }

    println!();
    println!("decisions from {}", log_path.display());
    println!(
        "total: {total} decisions across {} asset(s){}",
        by_asset.len(),
        if malformed > 0 {
            format!(" ({malformed} malformed lines skipped)")
        } else {
            String::new()
        }
    );
    println!();
    println!(
        "{:<6} {:>7} {:>6} {:>6} {:>6} {:>14} {:>10}",
        "asset", "total", "open", "close", "hold", "last_seen", "last_kind"
    );
    let now = Utc::now();
    for (asset, s) in &by_asset {
        println!(
            "{:<6} {:>7} {:>6} {:>6} {:>6} {:>14} {:>10}",
            asset,
            s.total,
            s.open,
            s.close,
            s.hold,
            format_age(now - s.last_seen),
            s.last_kind,
        );
    }

    Ok(())
}

fn format_age(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m ago", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn format_pct(d: Decimal) -> String {
    let pct = d * Decimal::from(100);
    format!("{:.1}%", pct)
}
