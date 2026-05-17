//! Persistence + read-side for `decisions.jsonl`, the append-only log of
//! decisions emitted by `perps-strategy::decide`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use perps_strategy::Decision;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// File name (within `state_dir`) for the append-only decisions log.
pub const DECISIONS_LOG_FILE: &str = "decisions.jsonl";

/// One row in `decisions.jsonl`. The inner `Decision` is flattened so the JSON
/// is grep-friendly: `kind`, `asset`, and (for Open) `side`/`notional_usd` are
/// top-level fields alongside `decided_at` and `signal_apy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub decided_at: DateTime<Utc>,
    pub signal_apy: Decimal,
    #[serde(flatten)]
    pub decision: Decision,
}

impl DecisionRecord {
    pub fn new(signal_apy: Decimal, decision: Decision) -> Self {
        Self {
            decided_at: Utc::now(),
            signal_apy,
            decision,
        }
    }
}

/// String name for the decision kind, suitable for human/table output.
pub fn kind_name(d: &Decision) -> &'static str {
    match d {
        Decision::Open { .. } => "open",
        Decision::Close { .. } => "close",
        Decision::Hold { .. } => "hold",
    }
}

/// Asset the decision applies to.
pub fn decision_asset(d: &Decision) -> &str {
    match d {
        Decision::Open { asset, .. }
        | Decision::Close { asset }
        | Decision::Hold { asset } => asset,
    }
}

/// Sync append. Decisions are infrequent (one per asset per poll tick — default
/// 5 min) and the line is ~150 bytes, so the cost of a blocking write inside
/// the poll-loop callback is negligible. Keeps the callback's signature simple
/// (no async closures, no spawned tasks).
pub fn append(path: &Path, record: &DecisionRecord) -> std::io::Result<()> {
    let line = serde_json::to_string(record)
        .expect("DecisionRecord serializes infallibly with derived Serialize");
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
    use perps_types::Side;
    use rust_decimal_macros::dec;
    use tempfile::TempDir;

    #[test]
    fn record_serializes_flattened() {
        let rec = DecisionRecord {
            decided_at: chrono::DateTime::parse_from_rfc3339("2026-05-17T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            signal_apy: dec!(0.293),
            decision: Decision::Open {
                asset: "BTC".into(),
                side: Side::Short,
                notional_usd: dec!(1000),
            },
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(v["decided_at"], "2026-05-17T00:00:00Z");
        assert_eq!(v["signal_apy"], "0.293");
        assert_eq!(v["kind"], "open");
        assert_eq!(v["asset"], "BTC");
        assert_eq!(v["side"], "short");
        assert_eq!(v["notional_usd"], "1000");
    }

    #[test]
    fn append_roundtrips_through_jsonl() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("decisions.jsonl");

        let rec1 = DecisionRecord::new(
            dec!(0.20),
            Decision::Open {
                asset: "BTC".into(),
                side: Side::Short,
                notional_usd: dec!(500),
            },
        );
        let rec2 = DecisionRecord::new(dec!(0.01), Decision::Close { asset: "BTC".into() });
        let rec3 = DecisionRecord::new(dec!(0.05), Decision::Hold { asset: "ETH".into() });

        append(&path, &rec1).unwrap();
        append(&path, &rec2).unwrap();
        append(&path, &rec3).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);

        let parsed: Vec<DecisionRecord> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(matches!(parsed[0].decision, Decision::Open { .. }));
        assert!(matches!(parsed[1].decision, Decision::Close { .. }));
        assert!(matches!(parsed[2].decision, Decision::Hold { .. }));
    }
}
