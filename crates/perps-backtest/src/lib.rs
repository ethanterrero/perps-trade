//! Historical funding-rate replay.
//!
//! Reads `state/funding.jsonl` (or pulled history), applies the same strategy rules,
//! and reports realized PnL net of (modeled) fees and slippage.
