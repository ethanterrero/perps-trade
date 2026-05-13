# perps-trade

Delta-neutral funding-rate harvester for perpetual futures markets.

**Status:** scaffolding. No live trading. No keys wired. `cargo build` is green from commit #1.

## Thesis

Perpetual futures use periodic funding payments to keep contracts tethered to spot. In bull/sideways markets, funding usually flows long → short. By holding a long spot (or long negative-funding perp) and short perp (or short positive-funding perp) of equal notional, net delta is zero and funding accrues as yield.

See [docs/research/delta-neutral-primer.md](docs/research/delta-neutral-primer.md) for the full mechanic.

## Layout

```
crates/
  perps-types       Shared domain types (Position, FundingRate, Venue, Order)
  perps-config      Config loading (toml + env)
  perps-venues      Exchange API clients (Hyperliquid first)
  perps-funding     Funding rate fetcher + signal generation
  perps-strategy    Delta-neutral entry/exit logic
  perps-risk        Margin monitor, liquidation buffer, position sizing
  perps-executor    Order placement, fills, rebalancing
  perps-backtest    Historical funding-rate simulation
  perps-bot         Main binary
config/             Runtime config (default.toml)
docs/research/      Design notes
ops/launchd/        macOS service files
```

## Build

```bash
cargo build
cargo run -p perps-bot
```

## Roadmap

See [ROADMAP.md](ROADMAP.md). MVP target is single-venue funding observation on Hyperliquid testnet — no orders placed.
