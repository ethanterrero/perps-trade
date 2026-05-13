//! Funding-rate polling and signal generation.
//!
//! Phase 1: poll `VenueClient::funding_rate` for each configured asset on a fixed cadence,
//! log observations, and persist to JSONL for later analysis.
