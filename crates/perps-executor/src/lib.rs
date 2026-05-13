//! Order placement, fill tracking, and notional-balance rebalancing.
//!
//! Two modes:
//!   - DryRun: simulate fills at venue mid, no orders sent (phase 2 paper trading).
//!   - Live:   send signed orders to the venue (phase 4+).
