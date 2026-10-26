use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use clap::{Parser, Subcommand};
use perps_executor::{simulate_fill, spot_fill_price, FillIntent, FILLS_LOG_FILE};
use perps_funding::FUNDING_LOG_FILE;
use perps_risk::{liquidation_buffer_pct, DEFAULT_MAINTENANCE_MARGIN_RATIO};
use perps_strategy::{decide, Decision, FundingSignal, FundingWindow, Thresholds};
use perps_types::{FeeSchedule, Instrument, MarketSnapshot, Order, Position, Side};
use perps_venues::hyperliquid::HyperliquidClient;
use perps_venues::VenueClient;
use rust_decimal::Decimal;
use tracing::{info, warn};
use uuid::Uuid;

use crate::decisions::{DecisionRecord, DECISIONS_LOG_FILE};

mod backtest;
mod decisions;
mod digest;
mod flatten;
mod pnl;
mod stats;

#[derive(Parser, Debug)]
#[command(name = "perps-bot", about = "Hedged funding-carry harvester (short perp + long spot)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the funding-rate poll loop (Phase 1 default).
    Run(RunArgs),
    /// Summarize observations recorded in state/funding.jsonl.
    Stats(StatsArgs),
    /// Print a paper-trade PnL digest: realized + unrealized + funding - fees per asset.
    Digest(DigestArgs),
    /// Close every open leg by appending a Close fill at the current mid.
    /// Stop the live daemon first to avoid racing on fills.jsonl.
    Flatten(FlattenArgs),
    /// Replay venue funding history through the strategy, net of fees and slippage.
    Backtest(backtest::BacktestArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Path to config directory.
    #[arg(long, default_value = "config")]
    config_dir: String,
    /// Required acknowledgement when running with `venue.hyperliquid.dry_run = false`.
    /// Belt-and-suspenders: even a misconfigured TOML can't enable live trading
    /// without an explicit flag on the command line.
    #[arg(long, default_value_t = false)]
    allow_live: bool,
}

#[derive(Parser, Debug)]
struct StatsArgs {
    /// Directory containing the funding JSONL log.
    #[arg(long, default_value = "state")]
    state_dir: PathBuf,
}

#[derive(Parser, Debug)]
struct DigestArgs {
    /// Directory containing the funding / decisions / fills JSONL logs.
    #[arg(long, default_value = "state")]
    state_dir: PathBuf,
}

#[derive(Parser, Debug)]
struct FlattenArgs {
    /// Path to config directory (used for the venue URL).
    #[arg(long, default_value = "config")]
    config_dir: String,
    /// Directory containing fills.jsonl.
    #[arg(long, default_value = "state")]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run(RunArgs {
        config_dir: "config".into(),
        allow_live: false,
    })) {
        Command::Run(args) => run(args).await,
        Command::Stats(args) => stats::run(&args.state_dir),
        Command::Digest(args) => digest::run(&args.state_dir),
        Command::Flatten(args) => flatten::run(&args.config_dir, &args.state_dir).await,
        Command::Backtest(args) => backtest::run(args).await,
    }
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let settings = perps_config::load(&args.config_dir)?;

    info!(
        venue_url = %settings.venue.hyperliquid.api_url,
        testnet = settings.venue.hyperliquid.testnet,
        assets = ?settings.strategy.assets,
        interval_secs = settings.strategy.decision_interval_seconds,
        state_dir = %settings.telemetry.state_dir,
        "perps-bot starting funding poller"
    );

    // Dry-run vs live: belt-and-suspenders gating. Both config (`dry_run = false`)
    // AND the `--allow-live` CLI flag must be present to enable signed orders.
    // Either alone refuses to proceed, so a misread config or a forgotten flag
    // can't surprise us into live trading.
    let dry_run = settings.venue.hyperliquid.dry_run;
    let spot_tokens = &settings.venue.hyperliquid.spot_tokens;
    let client: Arc<dyn VenueClient> = match (dry_run, args.allow_live) {
        (true, _) => {
            info!("============================================================");
            info!("DRY-RUN mode — fills simulated, no orders sent to the venue");
            info!("============================================================");
            Arc::new(
                HyperliquidClient::new(&settings.venue.hyperliquid.api_url)
                    .with_spot_tokens(spot_tokens),
            )
        }
        (false, false) => {
            anyhow::bail!(
                "LIVE TRADING refused: venue.hyperliquid.dry_run=false but --allow-live is not set. \
                 Pass --allow-live on the run command to confirm you intend real orders."
            );
        }
        (false, true) => {
            let secret_key = settings
                .venue
                .hyperliquid
                .secret_key
                .as_deref()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "dry_run=false requires venue.hyperliquid.secret_key (or set it via the \
                         PERPS_VENUE__HYPERLIQUID__SECRET_KEY env var). Never commit a real key."
                    )
                })?;
            tracing::warn!("============================================================");
            tracing::warn!("LIVE TRADING ENABLED — REAL ORDERS WILL BE PLACED ON {}",
                settings.venue.hyperliquid.api_url);
            tracing::warn!("============================================================");
            Arc::new(
                HyperliquidClient::with_signer(
                    &settings.venue.hyperliquid.api_url,
                    secret_key,
                    settings.venue.hyperliquid.testnet,
                )
                .await
                .map_err(|e| anyhow::anyhow!("live client init: {e}"))?
                .with_spot_tokens(spot_tokens),
            )
        }
    };
    let state_dir = PathBuf::from(&settings.telemetry.state_dir);
    let funding_path = state_dir.join(FUNDING_LOG_FILE);
    let decisions_path = state_dir.join(DECISIONS_LOG_FILE);
    let interval = Duration::from_secs(settings.strategy.decision_interval_seconds);

    let thresholds = Thresholds {
        min_apy_to_enter: settings.strategy.min_funding_apy_to_enter,
        max_apy_to_exit: settings.strategy.max_funding_apy_to_exit,
    };
    let max_notional = settings.risk.max_position_usd;
    let window_hours = settings.strategy.signal_window_hours;
    let min_signal_hours = settings.strategy.min_signal_hours;
    let fill_ctx = FillCtx {
        fills_path: state_dir.join(FILLS_LOG_FILE),
        fees: settings.fees,
        min_liq_buffer: settings.risk.liq_buffer_pct,
        hedgeable: spot_tokens.keys().map(|a| a.to_ascii_uppercase()).collect(),
    };
    // Replay fills.jsonl so a launchd respawn picks up where the previous run
    // left off rather than seeing an empty portfolio and re-opening everything.
    let restored = pnl::restore_portfolio(&state_dir)?;
    let restored_count = restored.positions.len();
    if restored_count > 0 {
        info!(
            legs = restored_count,
            "restored portfolio from fills.jsonl"
        );
    }
    // PortfolioState lives in an Arc<Mutex<_>> so the poll-loop's `Fn` callback
    // (called serially from one tokio task) can read it for `decide` and mutate
    // it after a fill, without us changing the callback bound to FnMut.
    let portfolio = Arc::new(Mutex::new(restored));
    let windows = Arc::new(Mutex::new(
        seed_windows(
            client.as_ref(),
            &settings.strategy.assets,
            window_hours,
            min_signal_hours,
        )
        .await,
    ));

    let on_snapshot = {
        let portfolio = Arc::clone(&portfolio);
        let windows = Arc::clone(&windows);
        move |snapshot: &MarketSnapshot| {
            let fr = &snapshot.funding;
            let raw_apy = fr.annualized();
            let (smoothed, hours) = {
                let mut windows = windows.lock().expect("windows mutex poisoned");
                let window = windows
                    .entry(fr.asset.to_ascii_uppercase())
                    .or_insert_with(|| FundingWindow::new(window_hours, min_signal_hours));
                window.record(fr.observed_at, raw_apy);
                (window.mean(), window.hours())
            };
            let Some(apy) = smoothed else {
                info!(
                    asset = %fr.asset,
                    hours,
                    needed = min_signal_hours,
                    raw_apy = %raw_apy,
                    "funding window warming up, no decision"
                );
                return;
            };
            let signal = FundingSignal {
                venue: fr.venue,
                asset: fr.asset.clone(),
                apy,
            };
            let decision = {
                let p = portfolio.lock().expect("portfolio mutex poisoned");
                decide(&p, &signal, &thresholds, max_notional)
            };
            log_decision(&signal, raw_apy, &decision);

            match &decision {
                Decision::Open {
                    asset,
                    notional_usd,
                } => {
                    let legs = open_carry(&fill_ctx, snapshot, asset, *notional_usd);
                    let mut p = portfolio.lock().expect("portfolio mutex poisoned");
                    for leg in legs {
                        p.open(leg);
                    }
                }
                Decision::Close { asset } => {
                    let legs = portfolio
                        .lock()
                        .expect("portfolio mutex poisoned")
                        .close(asset);
                    if legs.is_empty() {
                        warn!(
                            asset = %asset,
                            "strategy emitted Close but portfolio had no position"
                        );
                    }
                    for leg in legs {
                        close_leg(&fill_ctx, snapshot, &leg);
                    }
                }
                Decision::Hold { .. } => {}
            }

            let record = DecisionRecord::new(signal.apy, Some(raw_apy), decision);
            if let Err(e) = decisions::append(&decisions_path, &record) {
                warn!(error = %e, "failed to persist decision");
            }
        }
    };

    perps_funding::poll_loop(
        client,
        settings.strategy.assets.clone(),
        interval,
        funding_path,
        on_snapshot,
    )
    .await?;
    Ok(())
}

/// Pre-fill each asset's funding window from venue history so the bot can act
/// on its first tick instead of waiting a full window. A failed fetch only
/// delays that asset: its window fills from live polls instead.
async fn seed_windows(
    client: &dyn VenueClient,
    assets: &[String],
    window_hours: u32,
    min_signal_hours: u32,
) -> HashMap<String, FundingWindow> {
    let end = Utc::now();
    let start = end - chrono::Duration::hours(i64::from(window_hours));
    let mut windows = HashMap::new();
    for asset in assets {
        let mut window = FundingWindow::new(window_hours, min_signal_hours);
        match client.funding_history(asset, start, end).await {
            Ok(history) => {
                for fr in &history {
                    window.record(fr.observed_at, fr.annualized());
                }
                info!(
                    asset = %asset,
                    hours = window.hours(),
                    smoothed_apy = ?window.mean(),
                    "seeded funding window from venue history"
                );
            }
            Err(e) => warn!(
                asset = %asset,
                error = %e,
                "funding history unavailable; decisions wait until the window fills"
            ),
        }
        windows.insert(asset.to_ascii_uppercase(), window);
    }
    windows
}

/// What the fill helpers need from config, gathered once at startup.
struct FillCtx {
    fills_path: PathBuf,
    fees: FeeSchedule,
    min_liq_buffer: Decimal,
    /// Assets (uppercase) with a configured spot token to hedge with.
    hedgeable: HashSet<String>,
}

/// Simulate opening a hedged carry position: short `notional_usd` of the perp
/// and buy the same base size on spot. Returns the legs opened, or nothing if
/// the asset can't be hedged or the risk gate vetoes it.
fn open_carry(
    ctx: &FillCtx,
    snapshot: &MarketSnapshot,
    asset: &str,
    notional_usd: Decimal,
) -> Vec<Position> {
    let perp_mid = snapshot.mid_price;
    if perp_mid <= Decimal::ZERO {
        warn!(asset = %asset, "mid price non-positive, skipping fill");
        return Vec::new();
    }
    if !ctx.hedgeable.contains(&asset.to_ascii_uppercase()) {
        warn!(
            asset = %asset,
            "refused open: no spot token configured to hedge with (venue.hyperliquid.spot_tokens)"
        );
        return Vec::new();
    }
    // Live trading isn't wired into this callback yet, so this is always a
    // simulation; a missing or broken spot quote (normal on testnet) falls
    // back to the perp mid rather than blocking paper trading.
    let spot_price = spot_fill_price(perp_mid, snapshot.spot_mid_price).unwrap_or_else(|| {
        warn!(
            asset = %asset,
            spot_mid = ?snapshot.spot_mid_price,
            perp_mid = %perp_mid,
            "no sane spot quote; simulating the spot leg at the perp mid"
        );
        perp_mid
    });

    let size = notional_usd / perp_mid;
    let perp_leg = Position {
        venue: snapshot.funding.venue,
        asset: asset.to_string(),
        instrument: Instrument::Perp,
        side: Side::Short,
        size,
        entry_price: perp_mid,
        leverage: Decimal::ONE,
        margin_used: notional_usd,
        liquidation_price: None,
    };
    let buffer = liquidation_buffer_pct(&perp_leg, perp_mid, DEFAULT_MAINTENANCE_MARGIN_RATIO);
    if buffer < ctx.min_liq_buffer {
        // Risk gate vetoes the open. We still persist the decision (strategy
        // wanted it); the absence of matching fills in fills.jsonl is the
        // audit trail.
        warn!(
            asset = %asset,
            buffer = %buffer,
            min_buffer = %ctx.min_liq_buffer,
            "refused open: liq buffer below threshold"
        );
        return Vec::new();
    }

    let mut legs = Vec::with_capacity(2);
    for (instrument, side, price) in [
        (Instrument::Perp, Side::Short, perp_mid),
        (Instrument::Spot, Side::Long, spot_price),
    ] {
        let order = Order {
            venue: snapshot.funding.venue,
            asset: asset.to_string(),
            instrument,
            side,
            size,
            limit_price: None,
            reduce_only: false,
            client_id: Uuid::new_v4(),
        };
        let fill = simulate_fill(&order, price, FillIntent::Open, ctx.fees.taker_rate(instrument));
        info!(
            asset = %fill.asset,
            instrument = ?fill.instrument,
            side = ?fill.side,
            size = %fill.size,
            price = %fill.price,
            notional_usd = %fill.notional_usd,
            fees_usd = %fill.fees_usd,
            liq_buffer = %buffer,
            intent = "open",
            "simulated fill"
        );
        persist_fill(&ctx.fills_path, &fill);
        legs.push(Position {
            venue: fill.venue,
            asset: fill.asset.clone(),
            instrument,
            side,
            size: fill.size,
            entry_price: fill.price,
            leverage: Decimal::ONE,
            margin_used: fill.notional_usd,
            liquidation_price: None,
        });
    }
    legs
}

/// Simulate closing one leg at its current mid.
fn close_leg(ctx: &FillCtx, snapshot: &MarketSnapshot, leg: &Position) {
    let price = match leg.instrument {
        Instrument::Perp => snapshot.mid_price,
        Instrument::Spot => spot_fill_price(snapshot.mid_price, snapshot.spot_mid_price)
            .unwrap_or(snapshot.mid_price),
    };
    let order = Order {
        venue: leg.venue,
        asset: leg.asset.clone(),
        instrument: leg.instrument,
        side: leg.side.flip(),
        size: leg.size,
        limit_price: None,
        reduce_only: true,
        client_id: Uuid::new_v4(),
    };
    let fill = simulate_fill(&order, price, FillIntent::Close, ctx.fees.taker_rate(leg.instrument));
    info!(
        asset = %fill.asset,
        instrument = ?fill.instrument,
        side = ?fill.side,
        size = %fill.size,
        price = %fill.price,
        notional_usd = %fill.notional_usd,
        fees_usd = %fill.fees_usd,
        intent = "close",
        "simulated fill"
    );
    persist_fill(&ctx.fills_path, &fill);
}

fn persist_fill(path: &Path, fill: &perps_executor::Fill) {
    if let Err(e) = perps_executor::append(path, fill) {
        warn!(error = %e, "failed to persist fill");
    }
}

fn log_decision(signal: &FundingSignal, raw_apy: Decimal, decision: &Decision) {
    match decision {
        Decision::Open {
            asset,
            notional_usd,
        } => info!(
            asset = %asset,
            apy = %signal.apy,
            raw_apy = %raw_apy,
            notional_usd = %notional_usd,
            kind = "open",
            "dry-run decision"
        ),
        Decision::Close { asset } => info!(
            asset = %asset,
            apy = %signal.apy,
            raw_apy = %raw_apy,
            kind = "close",
            "dry-run decision"
        ),
        Decision::Hold { asset } => info!(
            asset = %asset,
            apy = %signal.apy,
            raw_apy = %raw_apy,
            kind = "hold",
            "dry-run decision"
        ),
    }
}
