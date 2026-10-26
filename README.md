# perps-trade

Hedged funding-carry harvester for perpetual futures markets, written in Rust: short the perp, buy the same size on spot, collect funding.

> **Status:** Phase 2 complete — paper-trading on Hyperliquid testnet end-to-end. No live trading wired. See [ROADMAP.md](ROADMAP.md) for what's next.

## This is research code

Trading code is dangerous. This bot:

- Has only been exercised against Hyperliquid testnet.
- Ships with `dry_run = true` by default; live orders require **both** `dry_run = false` in config AND `--allow-live` on the `run` command. Either gate missing refuses to start.
- Refuse-open and `flatten` kill-switch are wired; per-tick risk monitoring with alerting is still TODO.
- The spot hedge leg is simulated only: live `place_order` refuses spot orders until spot routing is wired.
- Is not production-tested.

Don't point this at mainnet expecting it to work safely, and don't load real keys into it.

## Thesis

Perpetual futures use periodic funding payments to keep contracts tethered to spot. In bull/sideways markets, funding usually flows long → short. By holding a short perp and a long spot position of equal size, net delta is zero and funding accrues as yield.

The bot only runs the trade one way. Harvesting negative funding would need short spot, which Hyperliquid doesn't offer, so negative funding means stay flat — never an unhedged long.

Entry and exit use a trailing 7-day mean of annualized funding, seeded from venue history at startup, not the latest hourly print. On a year of mainnet data the raw hourly signal re-entered 100–350 times per asset and paid several times more in fees than it collected (see [Backtest](#backtest)).

See [docs/research/delta-neutral-primer.md](docs/research/delta-neutral-primer.md) for the full mechanic.

## Layout

```
crates/
  perps-types       Domain types (Position, FundingRate, MarketSnapshot, Order, Side, Instrument, FeeSchedule)
  perps-config      Config loading (toml + env-var overrides)
  perps-venues      Exchange API clients (Hyperliquid first)
  perps-funding     Funding-rate polling, JSONL persistence, observation callback
  perps-strategy    Trailing funding window + carry entry/exit rule (pure)
  perps-risk        Notional / margin / liquidation-price compute
  perps-executor    Dry-run fill simulation with taker fees; fills JSONL
  perps-backtest    Funding-history replay, net of fees and slippage
  perps-bot         CLI binary: run / stats / digest / flatten / backtest
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

The bot polls Hyperliquid testnet's funding endpoint every `decision_interval_seconds` (default 300s = 5 min), folds each observation into the asset's trailing funding window, runs the smoothed signal through the strategy, simulates both legs' fills for `Open` / `Close` decisions, and persists three JSONL streams under `state/`:

- `funding.jsonl` — every observed funding rate + perp mid (and spot mid, when quoted) per asset
- `decisions.jsonl` — every dry-run decision (`open` / `hold` / `close`) with the smoothed and raw APY
- `fills.jsonl` — every simulated fill (`open` or `close` intent, `perp` or `spot` leg, with fees)

Testnet spot books are mostly empty, so in dry-run the spot leg fills at the perp mid whenever the spot quote is missing or more than 2% away (logged as a warning).

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
perps-bot run       # the poll loop (also the default if no subcommand)
perps-bot stats     # tabular summary of funding / decisions / fills streams
perps-bot digest    # per-asset PnL: realized + unrealized + funding - fees, plus per-leg liq metrics
perps-bot flatten   # kill switch: close every open leg (stop the daemon first)
perps-bot backtest  # replay mainnet funding history through the strategy, net of costs
```

Example `digest` output after a 21-second testnet smoke run (one hedged BTC position):

```
period: 2026-10-26T15:39:23Z → 2026-10-26T15:39:44Z (21s)
fills: 2  observations: 6  open legs: 2  closed legs: 0

asset   trades  open     realized   unrealized      funding         fees        total
BTC          0     1           $0           $0      $0.0039        $1.15     -$1.1461

totals: realized $0  unrealized $0  funding $0.0039  fees $1.15  net -$1.1461

open legs:
asset  leg   side             size        entry         mark      liq_price     buffer
BTC    spot  long         0.012311      81229.5      81228.5              -          -
BTC    perp  short        0.012311      81229.5      81228.5      162052.85      99.5%
```

### Backtest

`perps-bot backtest` pulls funding history from Hyperliquid **mainnet** (public `/info` endpoint, no keys — testnet funding isn't real market data) and replays it through the same `FundingWindow` + `decide` the bot runs, charging fees and slippage on both legs of every entry and exit:

```bash
perps-bot backtest                                   # config thresholds, strategy.assets, 365 days
perps-bot backtest --assets BTC,ETH,SOL,HYPE --enter=0.05,0.08,0.10 --exit=-0.05,0,0.02
```

Each asset prints one row per threshold pair plus two baselines: `raw hourly` (same rule on the latest hourly print) and `always on` (enter once, never exit). One year to 2026-10-26, $1000/leg, base-tier fees, 1 bp slippage:

| rule | BTC | ETH | SOL | HYPE |
|---|---|---|---|---|
| raw hourly, 10% / 2% | −50.3% | −46.8% | −66.2% | −83.0% |
| 7-day mean, 10% / 2% (config default) | 3.1% | 3.8% | 0.7% | 7.4% |
| 7-day mean, 8% / −5% | 4.7% | 4.5% | 1.7% | 9.2% |
| always on | 5.3% | 6.0% | 0.4% | 9.2% |

Returns are per year on notional per leg. Price PnL is taken as zero (equal-size legs); perp/spot basis isn't modelled.

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
cargo test                # ~75 unit tests, sub-second
cargo build -p perps-bot  # just the binary
```

CI runs `cargo build --verbose` + `cargo test --verbose` on push and PR to `main` ([.github/workflows/rust.yml](.github/workflows/rust.yml)).

Internal decision log lives in [devlog.md](devlog.md) — append-only, newest at top. Use it for decisions and surprises that aren't obvious from the diff.

## License

No license file yet. Until one is added the default is "all rights reserved" by the repository owner. Open an issue if you want to use the code for anything.
