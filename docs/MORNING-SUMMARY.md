# Morning briefing — 2026-06-01

## TL;DR

The bot is well past scaffolding — it observes funding, decides, simulates fills, tracks a
portfolio, attributes PnL, enforces a refuse-open gate, has a kill switch, and has live order
signing wired (gated behind two locks). **But it is not actually delta-neutral.** It opens a
single perp leg and carries full price exposure. Tonight I added the missing accounting
primitive — **net-delta tracking** — so we can *see* exactly how directional the book is, and
I've laid out the sequenced work to wire the hedge leg and start trading delta-0 for real.

Branch `claude/net-delta-accounting` → PR opened (link at bottom). 52 tests pass (was 48).

## The core finding

I ran the new digest against the 3.5-day testnet paper run already on disk:

```
period: 2026-05-18 → 2026-05-21 (3d14h)
fills: 6  observations: 2078  open positions: 2  closed trades: 2

totals: realized $3.42  unrealized -$16.51  funding $25.49  net $12.41

open positions:
asset  side    size       entry      mark      liq_price   buffer    delta_usd
ETH    short   0.469825   2128.45    2147.3     4246.26     97.7%   -$1008.86
BTC    short   0.012951   77214      77805      154041.93   98.0%   -$1007.65

net delta: -$2016.51 (gross $2016.51)
  ⚠ book is directional, not delta-neutral — hedge leg not yet wired (single-leg perp)
```

Read this carefully — it's the whole thesis in one screen:

- **Funding earned: +$25.49.** The yield engine works. On ~$2k of notional over 3.5 days that's
  a big annualized number (testnet funding runs hot, so discount it), but the *sign and
  mechanism* are validated: shorting positive-funding perps accrues funding.
- **Unrealized price PnL: −$16.51.** This is the problem. It's pure directional noise — BTC and
  ETH happened to tick up, and because we're short-only with no hedge, we ate it. In a real
  delta-neutral book this number should hover near zero. Half our funding got eaten by an
  unhedged price wiggle.
- **Net delta: −$2016.51, gross $2016.51 → the book is 100% directional.** We are running a
  leveraged short, not a funding harvester. We've been getting paid to take a directional bet,
  not to be market-neutral.

The fix is the second leg. That's the entire job from here.

## What I shipped tonight

A small, self-contained PR that adds the delta primitive without touching the (working) fill /
pairing / restore paths:

- `perps-types::Position::signed_notional(mark)` — +notional for a Long, −notional for a Short.
  A long-spot / short-perp pair of equal notional sums to zero. (1 new test)
- `perps-risk::net_delta_usd(positions, marks)` — portfolio net delta in USD. (2 new tests)
- `perps-risk::hedge_position_for(perp, mark)` — given a perp leg, returns the spot hedge that
  neutralizes it (opposite side, equal notional, 1x). This is the primitive the executor will
  call when we wire the second leg. (covered by the hedged-pair test)
- `perps-bot digest` — new `delta_usd` column per open position, a `net delta / gross` summary
  line, and a loud warning when the book is materially directional (≥1% of gross).

Deliberately **not** done tonight (to avoid a half-finished hedge path): actually placing the
spot leg, tagging fills with an instrument, or splitting PnL pairing by leg. Those are the next
PRs, sequenced below. Shipping the accounting first means every subsequent PR can be judged
against "did net delta move toward zero."

## The plan to reach delta-0 trading

Six steps. Each ends with something observable, same discipline as the existing ROADMAP.

### PR 1 — Tag the instrument (Perp vs Spot)  *(small, mechanical)*
Add `Instrument { Perp, Spot }` to `Order` and `Fill` (and optionally `Position`), `#[serde(default = Perp)]`
so existing `fills.jsonl` still deserializes. Key `pnl::pair_fills` by `(asset, instrument)` so a
BTC-perp short and a BTC-spot long are tracked as two independent legs instead of colliding.
**Exit:** existing digest output unchanged for old logs; new fills carry an instrument tag.

### PR 2 — Simulate the spot leg in the run loop  *(the real delta-neutral paper trade)*
On a perp `Open`, also simulate the spot hedge fill via `hedge_position_for`, persist both legs,
seed both into the portfolio. On `Close`, unwind both. Use the perp mid as the spot-price proxy
for now (testnet has no deep spot book; note the basis approximation in the devlog).
**Exit:** a fresh paper run shows `net delta ≈ $0` in the digest and `unrealized` collapses toward
zero while `funding` keeps accruing. This is the headline proof that we're market-neutral.

### PR 3 — Hedge-drift rebalancing  *(risk hardening, ROADMAP Phase 3 item)*
Price moves unbalance the legs (primer §"Hedge drift"). Add a per-tick check: if
`|net delta| / gross` exceeds a configurable band (e.g. 2%), emit a rebalancing fill on the
smaller leg. Config: `risk.max_delta_drift_pct`. Watch for fee churn — only rebalance outside a
dead band.
**Exit:** inject a synthetic 10% price move in a smoke test; confirm the bot rebalances back
inside the band and logs the cost.

### PR 4 — Real spot venue + basis  *(unblocks live, ROADMAP Phase 4 dependency)*
Decide where the spot leg lives. Cleanest single-venue option: Hyperliquid spot (HIP-1) for
assets that have it; otherwise the spot leg is a different venue (Phase 5 territory). Wire a
`spot_snapshot(asset)` read so we hedge at the real spot price, not the perp mid, and record the
basis. This is the honest version of PR 2's approximation.
**Exit:** digest shows perp mid, spot price, and basis side-by-side; net delta uses real spot.

### PR 5 — Reconciliation against the exchange  *(ROADMAP Phase 4)*
Before any real money: a `perps-bot reconcile` that pulls `clearinghouseState` (perp positions)
and spot balances and diffs them against the bot's reconstructed portfolio. `account_address` is
already plumbed in config for exactly this. Daily, refuse to trade on a mismatch.
**Exit:** reconcile reports zero drift on a paper run; non-zero exit on injected divergence.

### PR 6 — Small-size mainnet, both legs  *(ROADMAP Phase 4 exit)*
Only after 1–4 have soaked on testnet for the ROADMAP's 2 weeks. Keys in macOS keychain (the
`secret_key` loader is stubbed for this), `max_position_usd` capped low (~$500/asset), both legs
live, reconcile in the loop, kill switch verified to flatten *both* legs.
**Exit:** 1 month live, net delta stays in-band, realized funding − fees ≈ the testnet estimate.

## What I'd want your call on

1. **Spot venue for the hedge.** Hyperliquid spot only lists a subset of assets and BTC/ETH spot
   depth is thin. The clean math wants a real spot book. Options: (a) Hyperliquid spot where it
   exists, accept thin assets; (b) treat the hedge as a second *perp* on another venue
   (perp-perp, primer §Variants) — easier liquidity, still neutral, but pulls Phase 5 forward;
   (c) CEX spot (Binance/Coinbase) — best depth, most integration work. My lean: (b) for the
   testnet proof (reuse the `VenueClient` trait), (c) for mainnet.
2. **Rebalance band.** What `|net delta|/gross` do you want to tolerate before paying fees to
   rebalance? I'd start at 2% and tune from soak data.
3. **Asset set.** Still `["BTC","ETH"]`. Both have funding but the delta-neutral edge is often
   fatter on alts with hotter funding — at the cost of higher maintenance margin and thinner
   hedges. Expand after the two-leg path works on majors.

## Quick reference

- Branch: `claude/net-delta-accounting` (PR link in the PR description / chat)
- Run the new digest: `cargo run -p perps-bot -- digest`
- Roadmap: [ROADMAP.md](../ROADMAP.md) — this plan slots into Phase 3→4
- Strategy primer: [docs/research/delta-neutral-primer.md](research/delta-neutral-primer.md) — §"Hedge drift" and §"Underestimated risks" are the ones that bite
- Devlog: [devlog.md](../devlog.md) — tonight's entry at top
