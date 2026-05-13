# Roadmap

Phased build. Each phase ends with something runnable and observable. No phase advances until the prior one has been observed in practice (not just compiled).

## Phase 0 — Scaffold (done at commit #1)

- Workspace builds clean.
- Domain types defined.
- Config loader reads `config/default.toml`.
- `perps-bot` binary runs and prints structured logs.
- No network calls. No keys.

## Phase 1 — Observe (Hyperliquid testnet)

- `perps-venues::hyperliquid` REST client: fetch meta, fetch funding history, fetch current funding.
- `perps-funding` polling loop: every 5 min, log funding rate per asset, annualized.
- `perps-bot` runs as a daemon and writes funding observations to `state/funding.jsonl`.
- Exit criterion: 48h of clean uptime, no panics, sane numbers.

## Phase 2 — Paper-trade delta-neutral

- `perps-strategy` decides entry/exit from funding signal (threshold-based to start).
- `perps-executor` in dry-run mode: simulate fills at mid, track simulated positions.
- `perps-risk` computes notional balance, simulated margin usage, would-be liquidation prices.
- Daily PnL attribution: funding earned vs. fees vs. slippage vs. hedge drift.
- Exit criterion: 2 weeks of paper trading on testnet, simulated PnL roughly matches reality (compare to CoinGlass funding rates).

## Phase 3 — Risk module hardening

- Real-time margin monitoring with alerts.
- Auto-rebalance when notional drift exceeds threshold.
- Liquidation buffer enforcement (refuse to enter if buffer < N%).
- Kill switch: a single command (or signal) that flattens all positions.
- Failure modes documented: what happens on API outage, on WebSocket disconnect, on partial fill.

## Phase 4 — Small-size mainnet (single venue)

- Hyperliquid mainnet, capped notional (e.g., $500 per asset).
- Real keys with trade-only permissions, stored in OS keychain not in env files.
- 24/7 ops via `ops/launchd/`.
- Daily reconciliation: bot state vs. exchange state.
- Exit criterion: 1 month live, no incidents, PnL within expected range.

## Phase 5 — Multi-venue arb

- Add second venue (Binance or Bybit) behind the `Venue` trait.
- Perp-perp arb when funding spreads diverge.
- Capital allocation across venues.

## Out of scope (for now)

- On-chain leg (Aave borrow, Lido stake) — defer until CEX leg is rock-solid.
- Spot-perp arb (the cash-and-carry variant) — same; defer.
- ML-driven funding forecasts — threshold rules first, fancy stuff later.
