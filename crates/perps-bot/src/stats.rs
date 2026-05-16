use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::Utc;
use perps_funding::FUNDING_LOG_FILE;
use perps_types::FundingRate;
use rust_decimal::Decimal;

/// Per-asset summary derived from the JSONL log.
struct AssetStats {
    count: usize,
    last: FundingRate,
    min_apy: Decimal,
    max_apy: Decimal,
}

pub fn run(state_dir: &Path) -> anyhow::Result<()> {
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
