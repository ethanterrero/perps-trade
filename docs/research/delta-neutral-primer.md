# Delta-neutral perp strategies and funding mechanics

Hold a position that doesn't care if price goes up or down, and instead extract yield from the funding mechanism.

## Funding rates

Perps don't expire, so to keep them tethered to spot, exchanges use a periodic funding payment between longs and shorts. Hyperliquid pays hourly; Binance and Bybit pay every 8 hours.

- Perp > spot → longs pay shorts (positive funding)
- Perp < spot → shorts pay longs (negative funding)

In bull markets, funding is typically positive because leveraged long demand outstrips short demand. That spread is the inefficiency being harvested.

## Cash-and-carry (the classic trade)

1. Buy $X of BTC on spot.
2. Short $X of BTC perp on a venue.
3. Net delta ≈ 0. Price moves cancel.
4. Collect funding from longs every interval.

```
PnL ≈ Σ(funding_rate × notional) − fees − slippage − borrow_cost
```

Historically 5–40% APY on majors depending on market regime.

## Variants

**Perp-perp arb across venues.** If Binance funding is +0.03% and Hyperliquid is −0.01% on the same asset, long the negative-funding venue and short the positive one. Still delta-neutral. Requires capital on both sides — capital efficiency drops.

**Stablecoin-margined vs coin-margined.** Coin-margined shorts (collateral is BTC) are tricky — collateral value moves with the asset, so hedge is imperfect. Default to stablecoin-margined for clean math.

**On-chain analogs.** Ethena's USDe is this trade at protocol scale (long ETH staked, short ETH perp on CEXes, distribute funding as yield).

## Underestimated risks

1. **Funding flips negative.** In bear/crab markets, funding sits negative for weeks. Need an exit rule.
2. **Liquidation on the short leg.** Spot and perp collateral live in different accounts. If asset rips 30%, the perp short can liquidate even though spot "covers" it. Cross-margin or auto-rebalancing matters.
3. **Hedge drift.** Price moves change notional balance. If BTC rips 20%, the short is now smaller (in $ terms) than the long. Rebalancing costs fees.
4. **Exchange risk.** FTX. Counterparty risk on whoever holds the hedge is real.
5. **Fee drag.** Taker fees, funding spread fees, gas — at small size these eat the yield entirely.

## How this bot maps to the trade

- `perps-funding` produces the signal (annualized funding rate per asset per venue).
- `perps-strategy` decides when to open/close based on a threshold (see `min_funding_apy_to_enter` in config).
- `perps-executor` places and rebalances orders.
- `perps-risk` enforces margin buffer and notional caps so we don't get liquidated on a wick.
- `perps-backtest` replays historical funding to estimate realized yield net of fees.
