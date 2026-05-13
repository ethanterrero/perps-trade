//! Delta-neutral entry/exit decision logic.
//!
//! Phase 2 surface (planned):
//!   enum Decision { Open(asset, side, notional), Close(asset), Hold }
//!   fn decide(state: &PortfolioState, signal: &FundingSignal, cfg: &StrategySettings) -> Decision
//!
//! Threshold rules first. Anything fancier waits until we have realized PnL to compare against.
