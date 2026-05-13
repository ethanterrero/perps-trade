# devlog

Append-only log of decisions, surprises, and changes that aren't obvious from the diff. Newest at top.

---

## 2026-05-12 — Scaffold

Initial workspace created. Rust workspace matching the pattern from `Kalshi-Weather-Bot` (`crates/weather-*`). Nine member crates, all stubbed, workspace builds clean.

Hyperliquid chosen as first venue because (a) it has a maintained Rust SDK, (b) funding is hourly which means faster feedback during dev, (c) testnet is well-documented.

Decision: no real keys in this repo, ever. Mainnet keys will live in macOS keychain and be loaded by `perps-config` at startup. Testnet keys are fine in `.env` (gitignored) but the scaffold doesn't load anything yet.

Decision: `Decimal` everywhere for money/size. No `f64` in domain types. Following the Kalshi pattern.
