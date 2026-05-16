# devlog

Append-only log of decisions, surprises, and changes that aren't obvious from the diff. Newest at top.

---

## 2026-05-15 — Ops plist, stats subcommand, env-override fix

Layered on top of the Phase 1 observe loop (entry below).

`ops/launchd/com.perps-trade.bot.plist` is the macOS launchd unit: `RunAtLoad`, `KeepAlive`, `ThrottleInterval=30` so a permanent failure doesn't tight-loop. Logs go to `~/Library/Logs/perps-trade/perps-bot.{out,err}.log`. Install/load steps live in a comment at the top of the plist. The plist invokes `perps-bot run` explicitly so default-args drift can't repurpose it.

`perps-bot` now has clap subcommands: `run` (the original poll loop) and `stats` (reads `state/funding.jsonl`, prints a compact per-asset table — count, last_seen, last_rate, last/min/max APY). Default-no-subcommand falls through to `run` so the plist and old invocations both work. Stats handles missing-file and malformed-line cases gracefully (the latter matters across crash boundaries).

`perps-funding` exposes `pub const FUNDING_LOG_FILE = "funding.jsonl"` so the run-path and stats-path can't drift on the filename.

`HyperliquidClient::new` now builds the reqwest client with a 15s timeout. Without it a hung API would stall the poll loop indefinitely instead of erroring per-tick.

**Gotcha:** `config 0.14`'s `Environment::with_prefix("PERPS")` silently does not match `PERPS_*` env vars unless you also set `.prefix_separator("_")`. Default prefix separator is empty, so it was looking for env vars like `PERPSRISK__MAX_POSITION_USD`. Fixed in `perps-config::load`. Worth keeping in mind if we ever add another `Environment` source.

Smoke test (3s interval, 8s window, three cycles):
```
asset   count      last_seen      last_rate   last_apy    min_apy    max_apy
BTC         3         0s ago   0.0000768397      67.3%      67.3%      67.4%
ETH         3         0s ago   0.0003974273     348.1%     348.1%     348.2%
```

Next step toward Phase 1 exit criterion (48h clean uptime): `cargo build --release`, load the plist, and just leave it.

---

## 2026-05-15 — Phase 1 observe loop is live

`HyperliquidClient::funding_rate` hits `POST /info` with `{"type":"metaAndAssetCtxs"}`. Response is positionally-aligned `(Meta, [AssetCtx])` where `meta.universe[i].name` pairs with `ctxs[i].funding`. A reusable `post_info<T>` helper handles network errors and non-2xx status (including the response body in the error so diagnostics aren't blind). `extract_funding` does the name-to-index lookup (case-insensitive) and parses the funding string into a `Decimal`.

`perps-funding::poll_loop` owns the cadence and the ctrl-c handling. Each tick fans out to `poll_once`, which iterates assets, structured-logs the observation, and appends one JSONL line per observation to the configured path. Per-asset errors are warned and skipped — the loop only exits on signal.

Observations are wrapped in an `Observation` struct that flattens `FundingRate` and adds a precomputed `annualized` field. Consumers reading the JSONL don't have to recompute it from `rate * 24*365 / interval_hours`. The flatten + extra-field shape is covered by a unit test.

`perps-bot::main` constructs an `Arc<dyn VenueClient>`, joins `state_dir` with the log filename, hands off to `poll_loop`. Tests cover both happy path and unknown-asset error in `hyperliquid.rs`.

Verified live on testnet — funding rates change between successive polls so we know the path through the system is reading fresh data, not a cached response.

---

## 2026-05-12 — Scaffold

Initial workspace created. Rust workspace matching the pattern from `Kalshi-Weather-Bot` (`crates/weather-*`). Nine member crates, all stubbed, workspace builds clean.

Hyperliquid chosen as first venue because (a) it has a maintained Rust SDK, (b) funding is hourly which means faster feedback during dev, (c) testnet is well-documented.

Decision: no real keys in this repo, ever. Mainnet keys will live in macOS keychain and be loaded by `perps-config` at startup. Testnet keys are fine in `.env` (gitignored) but the scaffold doesn't load anything yet.

Decision: `Decimal` everywhere for money/size. No `f64` in domain types. Following the Kalshi pattern.
