# perps-trade

Delta-neutral funding-rate harvester for perpetual futures markets, written in Rust.

> **Status:** Phase 2 complete — paper-trading on Hyperliquid testnet end-to-end. No live trading wired. See [ROADMAP.md](ROADMAP.md) for what's next.

## This is research code

Trading code is dangerous. This bot:

- Has only been exercised against Hyperliquid testnet.
- Has no order signing wired — it cannot submit a real order even if you point the config at mainnet.
- Has no liquidation enforcement gate or kill switch yet (Phase 3).
- Is not production-tested.

Don't point this at mainnet expecting it to work safely, and don't load real keys into it.

## Thesis

Perpetual futures use periodic funding payments to keep contracts tethered to spot. In bull/sideways markets, funding usually flows long → short. By holding a long spot (or long negative-funding perp) and a short perp (or short positive-funding perp) of equal notional, net delta is zero and funding accrues as yield.

See [docs/research/delta-neutral-primer.md](docs/research/delta-neutral-primer.md) for the full mechanic.

## Layout

```
crates/
  perps-types       Domain types (Position, FundingRate, MarketSnapshot, Order, Side, Venue)
  perps-config      Config loading (toml + env-var overrides)
  perps-venues      Exchange API clients (Hyperliquid first)
  perps-funding     Funding-rate polling, JSONL persistence, observation callback
  perps-strategy    Threshold-based entry/exit decision logic (pure)
  perps-risk        Notional / margin / liquidation-price compute
  perps-executor    Dry-run fill simulation; fills JSONL
  perps-backtest    Historical-funding simulation (stub for Phase 5)
  perps-bot         CLI binary: run / stats / digest subcommands
config/             Runtime config (default.toml)
docs/               Design notes, research
ops/launchd/        macOS launchd service files
devlog.md           Internal decision log (newest at top)
```

## Quickstart — paper-trade on Hyperliquid testnet

Requires Rust 1.70+ and macOS or Linux.

```bash
git clone https://github.com/ethanterrero/perps-trade.git
cd perps-trade
cargo build --release

# One-shot dry run; Ctrl-C to exit
./target/release/perps-bot run
```

The bot polls Hyperliquid testnet's funding endpoint every `decision_interval_seconds` (default 300s = 5 min), runs each observation through the strategy, simulates fills for `Open` / `Close` decisions, and persists three JSONL streams under `state/`:

- `funding.jsonl` — every observed funding rate + mid price per asset
- `decisions.jsonl` — every dry-run decision (`open` / `hold` / `close`)
- `fills.jsonl` — every simulated fill (`open` or `close` intent)

### Running unattended (macOS launchd)

```bash
# Edit ops/launchd/com.perps-trade.bot.plist — replace every `/Users/ethanterrero/...`
# path with your own checkout path and home directory.
mkdir -p ~/Library/Logs/perps-trade ~/Library/LaunchAgents
cp ops/launchd/com.perps-trade.bot.plist ~/Library/LaunchAgents/
launchctl load ~/Library/LaunchAgents/com.perps-trade.bot.plist

# Tail logs
tail -f ~/Library/Logs/perps-trade/perps-bot.out.log

# Stop / reload
launchctl unload ~/Library/LaunchAgents/com.perps-trade.bot.plist
launchctl load   ~/Library/LaunchAgents/com.perps-trade.bot.plist
```

The plist uses `KeepAlive` + a 30s `ThrottleInterval` so a crash is followed by a respawn but a permanent failure won't tight-loop.

## Subcommands

```bash
perps-bot run     # the poll loop (also the default if no subcommand)
perps-bot stats   # tabular summary of funding / decisions / fills streams
perps-bot digest  # per-asset PnL: realized + unrealized + funding, plus open-position liq metrics
```

Example `digest` output after a 9-second smoke run:

```
period: 2026-05-17T03:44:03Z → 2026-05-17T03:44:12Z (9s)
fills: 2  observations: 8  open positions: 2  closed trades: 0

asset   trades  open     realized   unrealized      funding        total
BTC          0     1           $0     -$0.1151           $0     -$0.1151
ETH          0     1           $0      $0.0457      $0.0008      $0.0465

totals: realized $0  unrealized -$0.0694  funding $0.0008  net -$0.0686

open positions:
asset  side       size        entry      mark      liq_price    buffer
BTC    short  0.012789       78193     78202      155995.04     99.5%
ETH    short  0.456976      2188.3    2188.2        4365.66     99.5%
```

## Config

Defaults are in [config/default.toml](config/default.toml). Override per environment with `config/$RUN_ENV.toml` or with `PERPS_*` env vars (double underscore is the path separator):

```bash
PERPS_STRATEGY__MIN_FUNDING_APY_TO_ENTER=0.15 ./target/release/perps-bot run
PERPS_RISK__MAX_POSITION_USD=500              ./target/release/perps-bot run
```

So `PERPS_STRATEGY__MIN_FUNDING_APY_TO_ENTER` maps to `strategy.min_funding_apy_to_enter` in the toml.

## Roadmap

See [ROADMAP.md](ROADMAP.md). Headline:

- **Phase 0–2 (done):** scaffolding, observe loop, strategy + executor dry-run, portfolio tracker, PnL attribution, risk compute.
- **Phase 3 (next):** real-time risk monitoring, refuse-open gate wired into executor, kill switch, failure-mode hardening.
- **Phase 4:** small-size mainnet, keys in OS keychain, daily reconciliation against the exchange.
- **Phase 5:** multi-venue support and cross-venue funding-spread arb.

## Development

```bash
cargo build               # workspace builds clean
cargo test                # ~47 unit tests, sub-second
cargo build -p perps-bot  # just the binary
```

CI runs `cargo build --verbose` + `cargo test --verbose` on push and PR to `main` ([.github/workflows/rust.yml](.github/workflows/rust.yml)).

Internal decision log lives in [devlog.md](devlog.md) — append-only, newest at top. Use it for decisions and surprises that aren't obvious from the diff.

## License

No license file yet. Until one is added the default is "all rights reserved" by the repository owner. Open an issue if you want to use the code for anything.
