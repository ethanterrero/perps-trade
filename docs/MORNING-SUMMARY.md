# Morning briefing — 2026-05-13

## What's on disk

Fresh Rust workspace at `~/Desktop/perps-trade`. Private repo at https://github.com/ethanterrero/perps-trade. One commit, on `main`, pushed. Build is green, two unit tests pass, `cargo run -p perps-bot` exits cleanly with structured JSON logs.

```bash
cd ~/Desktop/perps-trade
cargo build                # green
cargo test                 # 2 passing in perps-types
cargo run -p perps-bot     # prints config + exits
```

Layout matches the `Kalshi-Weather-Bot` pattern you already use — 9 crates under `crates/perps-*`, plus `config/`, `docs/research/`, `ops/launchd/`, `ROADMAP.md`, `devlog.md`.

## What actually has code vs. is a stub

- **Real code:**
  - `perps-types` — `Venue`, `Side`, `FundingRate` (with `annualized()`), `Position` (with `notional_usd()`), `Order`, `VenueError`. Decimals everywhere, no f64.
  - `perps-config` — loads `config/default.toml`, layers `config/{RUN_ENV}.toml`, then `PERPS_*` env vars. Override pattern: `PERPS_RISK__MAX_POSITION_USD=500`.
  - `perps-bot` — clap-parsed args, JSON tracing logs, prints loaded config and exits.
  - `perps-venues` — `VenueClient` async trait + `HyperliquidClient` struct skeleton. **All four methods return `not implemented`** — this is intentional.
- **Stubs (one-line `//!` doc comment, empty otherwise):** `perps-funding`, `perps-strategy`, `perps-risk`, `perps-executor`, `perps-backtest`.

## Where to pick up — Phase 1, first concrete task

Goal of Phase 1 (per [ROADMAP.md](../ROADMAP.md)): observe funding rates on Hyperliquid testnet for 48h with no panics. No trading.

**First task:** implement `HyperliquidClient::funding_rate(asset)` in [crates/perps-venues/src/hyperliquid.rs](../crates/perps-venues/src/hyperliquid.rs:23).

How: Hyperliquid's REST endpoint is `POST {api_url}/info` with a JSON body. For funding, the body is `{"type": "metaAndAssetCtxs"}` — the response includes a `funding` field per asset in the second array element. Parse and return a `FundingRate { interval_hours: 1, ... }`.

[Hyperliquid API docs — info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)

The official [hyperliquid-rust-sdk](https://github.com/hyperliquid-dex/hyperliquid-rust-sdk) crate exists if you want to skip writing the HTTP plumbing. Tradeoff: more deps, less control over types. My suggestion is to roll the read-only HTTP call yourself (it's ~30 lines with reqwest + serde) and only pull the SDK when you need EIP-712 signing for orders in Phase 2.

After that endpoint works, the next two steps are:
1. Add a `poll_loop(client, assets, interval)` in `perps-funding` that calls `funding_rate` for each asset and writes a JSONL line per observation to `state/funding.jsonl`.
2. Wire `perps-bot::main` to spawn that loop instead of exiting immediately.

That gets you to "Phase 1 exit criterion" once it survives 48h.

## Decisions baked in (so you don't have to re-decide them)

- **`Decimal` everywhere** for money/size. Never `f64` in domain types. Matches Kalshi.
- **Testnet first** — `config/default.toml` points at `api.hyperliquid-testnet.xyz`. Mainnet will be a separate `config/prod.toml` you create only when you're ready.
- **No keys in repo, ever.** Testnet keys can sit in a gitignored `.env`. Mainnet keys go in macOS keychain, loaded by `perps-config` at startup (not wired yet — that's a Phase 4 task).
- **JSON logs** by default (matches Kalshi's `ops/launchd/` style for log shipping later).
- **`PERPS_` env-var prefix** with `__` separator for path overrides.
- **Private repo, `ethanterrero` account.**

## Things I deliberately did NOT do

- No `.env.example` file — you'll create one when there's actually a secret to template.
- No CI workflow — add when you're past Phase 1 and the surface area is stable.
- No `Cargo.lock` committed analysis — Cargo committed one automatically since this is a binary workspace; that's correct.
- No `ops/launchd/com.perps-trade.plist` yet — that's Phase 4 when the bot runs 24/7.
- Didn't pre-add the `hyperliquid-rust-sdk` dependency — let you decide SDK vs. raw HTTP when you actually need to sign.

## Open questions for you when you sit down

1. **SDK or raw HTTP** for the Hyperliquid client? (See above — I'd go raw for reads, SDK only when signing is needed.)
2. **Asset list** in `config/default.toml` is `["BTC", "ETH"]`. Want to add SOL or others before Phase 1 starts collecting?
3. **State directory location** — currently `state/` (relative, gitignored). Fine, or do you want `~/Library/Application Support/perps-trade/state` to match macOS conventions like Kalshi might?

## Quick reference

- Repo: https://github.com/ethanterrero/perps-trade
- Local: `~/Desktop/perps-trade`
- Roadmap: [ROADMAP.md](../ROADMAP.md)
- Strategy primer: [docs/research/delta-neutral-primer.md](research/delta-neutral-primer.md)
- Devlog: [devlog.md](../devlog.md) — append decisions here, not in commits
