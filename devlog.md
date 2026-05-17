# devlog

Append-only log of decisions, surprises, and changes that aren't obvious from the diff. Newest at top.

---

## 2026-05-17 — Executor dry-run + MarketSnapshot + fills.jsonl

Phase 2 paper-trade pipeline is now end-to-end: funding observation → decide → simulate fill → persist Fill. Three JSONL streams (`funding.jsonl`, `decisions.jsonl`, `fills.jsonl`) and `perps-bot stats` shows all three.

**`MarketSnapshot { funding, mid_price }` replaces `funding_rate` on `VenueClient`.** Hyperliquid's `metaAndAssetCtxs` already returns both fields in the same response, so paying for two round-trips per asset per tick (one for funding, one for mid) was wasteful. The trait now has a single `market_snapshot(asset)` method returning both; `funding.jsonl` schema is unchanged (we still write the FundingRate piece via the existing `Observation` struct).

The `poll_loop` callback signature changed from `Fn(&FundingRate)` to `Fn(&MarketSnapshot)`. Sync, like before. The closure in `perps-bot::run` uses `snapshot.funding` for the decision and `snapshot.mid_price` for the fill — no extra async calls during the tick.

**`perps-executor::simulate_fill(order, mid, intent) -> Fill`** is pure: no fees, no slippage, no partial fills. Fills get a `FillIntent::{Open, Close}` tag set by the caller (the bot knows whether it's opening or closing; the executor doesn't need to figure it out from existing positions). `notional_usd` is stored on the Fill explicitly rather than re-computed by readers, so Decimal-scale mismatches don't surprise anyone.

**Known gap, intentional:** with no portfolio tracker yet, `PortfolioState` is always empty, so the strategy re-emits Open every tick and the bot re-fills. We end up with a `fills.jsonl` row per tick per asset even though in reality we'd only open once. Fixing this is PR #6's job (mutable PortfolioState across ticks; emit Hold/Close once a position exists).

Smoke run, 4s interval, 3 cycles, 2 assets — 6 observations / 6 decisions / 6 fills, all consistent.

```
fills from state/fills.jsonl
total: 6 fills across 2 asset(s)
asset    total  opens closes      last_seen  last_intent
BTC          3      3      0         0s ago         open
ETH          3      3      0         0s ago         open
```

Real testnet prices: BTC $78,091.50, ETH $2,185.10. So a $1,000 notional Short BTC produces a fill of size 0.01280549 BTC. The Decimal-divide leaves trailing digits — cosmetic but technically correct.

Tests: 21 across the workspace (was 15). Four new in `perps-executor` (limit vs. market fill price, intent passthrough, JSONL roundtrip) plus two in `perps-venues` (mid price parsing happy/null).

---

## 2026-05-17 — Persist decisions to JSONL + stats decisions section

`decisions.jsonl` is the new sibling to `funding.jsonl`. Every tick that emits a decision now produces a line like:

```json
{"decided_at":"2026-05-17T03:14:30.434Z","signal_apy":"0.1095000","kind":"open","asset":"BTC","side":"short","notional_usd":"1000"}
```

The wrapper `DecisionRecord { decided_at, signal_apy, decision }` flattens `Decision`'s own `kind`-tagged serialization so each row is grep-friendly: `kind`, `asset`, and (for `open`) `side`/`notional_usd` are top-level keys, not nested.

**API change: `Decision::Hold` now carries `asset`.** It was a unit variant, but every decision is logically per-asset and the JSONL shape was simpler if every variant exposes `asset` uniformly. Three strategy tests and the `log_decision` matcher updated; small public-API churn that I'd rather take now than after more callers exist.

**Persistence stays sync.** The poll-loop callback is `Fn(&FundingRate)` (not async), so `decisions::append` uses `std::fs` directly. Decisions fire at most once per asset per `decision_interval_seconds` (default 5 min) and the line is ~150 bytes — async/channels would be over-engineering. If we ever batch-write or need backpressure, switch then.

`perps-bot stats` grew a second section. Funding table prints first, then a decisions table with per-asset totals plus counts by kind and the latest decision. Empty/missing decisions.jsonl is handled silently — funding-only state still works for users on Phase 1.

Smoke-tested after a 3-cycle run: 6 funding observations + 6 decisions persisted, both tables rendered cleanly. Verifies the callback fires after the funding write (so we never have a decision without its corresponding observation on disk).

Tests: 15 across the workspace (was 13). Two new in `perps-bot::decisions` — JSON shape and JSONL roundtrip via `tempfile`.

---

## 2026-05-16 — Wire decide into perps-bot (dry-run)

The strategy stub is now plumbed into the run loop. Each successful funding fetch fans out to `decide` with an empty `PortfolioState` (we have no live positions yet) and the decision gets logged via structured tracing — no orders, no JSONL for decisions, just observability.

**Architecture call: `poll_loop` got a per-observation callback.** `perps-funding::poll_loop` now takes `on_observation: Fn(&FundingRate)` as a generic parameter and invokes it after each successful fetch is persisted. The alternative was to put strategy invocation inside `perps-funding`, but that'd make the observation crate depend on the decision crate — wrong direction. The callback keeps `perps-funding` ignorant of `perps-strategy`; `perps-bot` is where the two worlds meet.

**Mapping at the call site.** `perps-bot::run` builds a `Thresholds` from `StrategySettings` and reads `max_position_usd` from `RiskSettings`. The closure converts each `FundingRate → FundingSignal` (`apy = fr.annualized()`) and emits a single `dry-run decision` log line with `kind = open|close|hold`. The `kind` field matches the `Decision` serde tag so future log-querying is consistent across structured-log and decisions-JSONL once that arrives.

`log_decision` is split out as a separate function rather than inlined because the `Decision::Open` arm has three extra fields the others don't, and a single `info!` with `Option`-fields would be uglier than a match.

Smoke run against testnet — three cycles, two assets, six decisions. Sample line (BTC at ~29% APY, well above the 10% enter threshold):

```json
{"message":"dry-run decision","asset":"BTC","apy":"0.2939260320","side":"Short","notional_usd":"1000","kind":"open"}
```

**Not yet wired:** real positions in `PortfolioState`, decisions persisted to JSONL, the executor consuming them. Those are separate PRs — the next obvious one is decisions JSONL so we can build a `perps-bot stats` extension that reports decision history alongside funding history.

---

## 2026-05-16 — Phase 2 stub: perps-strategy::decide

Pure-logic stub for the delta-neutral entry/exit rule. `decide(state, signal, thresholds, max_notional) -> Decision` where `Decision` is `Open { asset, side, notional_usd } | Close { asset } | Hold`. No I/O, no async, no wiring into the bot yet — that's the next chunk once the observe loop has banked a couple of days of data.

Sign convention follows Hyperliquid funding: positive APY ⇒ longs pay shorts ⇒ collect by shorting; negative APY ⇒ shorts pay longs ⇒ collect by going long. Symmetric thresholds: `|apy| >= min_apy_to_enter` opens, `|apy| < max_apy_to_exit` closes. The dead zone between the two is deliberate so we don't churn when funding wobbles near a threshold.

**Decision: don't take `&StrategySettings` directly.** The roadmap sketched `decide(... cfg: &StrategySettings)` but importing `perps-config` into `perps-strategy` pulls in the loader (config crate, dotenvy, …) which the pure-logic crate doesn't need. Defined a local `Thresholds { min_apy_to_enter, max_apy_to_exit }` instead — the caller maps from settings at the call site. Cheap to change later if it becomes annoying.

`max_notional_usd` is passed in as a parameter rather than baked into `Thresholds`. It lives in `RiskSettings`, not `StrategySettings`, and conceptually sizing is the risk module's call, not the strategy's — the strategy just emits "open with at-most this notional" and risk gets final say later.

Decisions serialize with a `kind` tag (`{"kind":"open","asset":"BTC",...}`) so future executor logs are self-describing. 8 unit tests cover boundary, symmetric sign, dead-zone hold, case-insensitive asset matching, and the JSON shape.

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
