use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use perps_executor::{simulate_fill, FillIntent, FILLS_LOG_FILE};
use perps_funding::FUNDING_LOG_FILE;
use perps_risk::{liquidation_buffer_pct, DEFAULT_MAINTENANCE_MARGIN_RATIO};
use perps_strategy::{decide, Decision, FundingSignal, Thresholds};
use perps_types::{MarketSnapshot, Order, Position};
use perps_venues::hyperliquid::HyperliquidClient;
use perps_venues::VenueClient;
use rust_decimal::Decimal;
use tracing::{info, warn};
use uuid::Uuid;

use crate::decisions::{DecisionRecord, DECISIONS_LOG_FILE};

mod decisions;
mod digest;
mod pnl;
mod stats;

#[derive(Parser, Debug)]
#[command(name = "perps-bot", about = "Delta-neutral funding-rate harvester")]
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
    /// Print a paper-trade PnL digest: realized + unrealized + funding per asset.
    Digest(DigestArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Path to config directory.
    #[arg(long, default_value = "config")]
    config_dir: String,
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run(RunArgs {
        config_dir: "config".into(),
    })) {
        Command::Run(args) => run(args).await,
        Command::Stats(args) => stats::run(&args.state_dir),
        Command::Digest(args) => digest::run(&args.state_dir),
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

    let client: Arc<dyn VenueClient> =
        Arc::new(HyperliquidClient::new(&settings.venue.hyperliquid.api_url));
    let state_dir = PathBuf::from(&settings.telemetry.state_dir);
    let funding_path = state_dir.join(FUNDING_LOG_FILE);
    let decisions_path = state_dir.join(DECISIONS_LOG_FILE);
    let fills_path = state_dir.join(FILLS_LOG_FILE);
    let interval = Duration::from_secs(settings.strategy.decision_interval_seconds);

    let thresholds = Thresholds {
        min_apy_to_enter: settings.strategy.min_funding_apy_to_enter,
        max_apy_to_exit: settings.strategy.max_funding_apy_to_exit,
    };
    let max_notional = settings.risk.max_position_usd;
    let min_liq_buffer = settings.risk.liq_buffer_pct;
    // Replay fills.jsonl so a launchd respawn picks up where the previous run
    // left off rather than seeing an empty portfolio and re-opening everything.
    let restored = pnl::restore_portfolio(&state_dir)?;
    let restored_count = restored.positions.len();
    if restored_count > 0 {
        info!(
            positions = restored_count,
            "restored portfolio from fills.jsonl"
        );
    }
    // PortfolioState lives in an Arc<Mutex<_>> so the poll-loop's `Fn` callback
    // (called serially from one tokio task) can read it for `decide` and mutate
    // it after a fill, without us changing the callback bound to FnMut.
    let portfolio = Arc::new(Mutex::new(restored));

    let on_snapshot = {
        let portfolio = Arc::clone(&portfolio);
        move |snapshot: &MarketSnapshot| {
            let fr = &snapshot.funding;
            let signal = FundingSignal {
                venue: fr.venue,
                asset: fr.asset.clone(),
                apy: fr.annualized(),
            };
            let decision = {
                let p = portfolio.lock().expect("portfolio mutex poisoned");
                decide(&p, &signal, &thresholds, max_notional)
            };
            log_decision(&signal, &decision);

            match &decision {
                Decision::Open {
                    asset,
                    side,
                    notional_usd,
                } => {
                    if snapshot.mid_price <= Decimal::ZERO {
                        warn!(asset = %asset, "mid price non-positive, skipping fill");
                    } else {
                        let size = notional_usd / snapshot.mid_price;
                        let prospective = Position {
                            venue: fr.venue,
                            asset: asset.clone(),
                            side: *side,
                            size,
                            entry_price: snapshot.mid_price,
                            leverage: Decimal::ONE,
                            margin_used: *notional_usd,
                            liquidation_price: None,
                        };
                        let buffer = liquidation_buffer_pct(
                            &prospective,
                            snapshot.mid_price,
                            DEFAULT_MAINTENANCE_MARGIN_RATIO,
                        );
                        if buffer < min_liq_buffer {
                            // Risk gate vetoes the open. We still persist the
                            // decision (strategy wanted it); the absence of a
                            // matching fill in fills.jsonl is the audit trail.
                            warn!(
                                asset = %asset,
                                side = ?side,
                                buffer = %buffer,
                                min_buffer = %min_liq_buffer,
                                "refused open: liq buffer below threshold"
                            );
                        } else {
                            let order = Order {
                                venue: fr.venue,
                                asset: asset.clone(),
                                side: *side,
                                size,
                                limit_price: None,
                                reduce_only: false,
                                client_id: Uuid::new_v4(),
                            };
                            let fill = simulate_fill(&order, snapshot.mid_price, FillIntent::Open);
                            info!(
                                asset = %fill.asset,
                                side = ?fill.side,
                                size = %fill.size,
                                price = %fill.price,
                                notional_usd = %fill.notional_usd,
                                liq_buffer = %buffer,
                                intent = "open",
                                "simulated fill"
                            );
                            if let Err(e) = perps_executor::append(&fills_path, &fill) {
                                warn!(error = %e, "failed to persist fill");
                            }
                            let position = Position {
                                venue: fr.venue,
                                asset: asset.clone(),
                                side: *side,
                                size: fill.size,
                                entry_price: fill.price,
                                leverage: Decimal::ONE,
                                margin_used: fill.notional_usd,
                                liquidation_price: None,
                            };
                            portfolio
                                .lock()
                                .expect("portfolio mutex poisoned")
                                .open(position);
                        }
                    }
                }
                Decision::Close { asset } => {
                    let closed = portfolio
                        .lock()
                        .expect("portfolio mutex poisoned")
                        .close(asset);
                    match closed {
                        Some(pos) => {
                            let order = Order {
                                venue: pos.venue,
                                asset: pos.asset.clone(),
                                side: pos.side.flip(),
                                size: pos.size,
                                limit_price: None,
                                reduce_only: true,
                                client_id: Uuid::new_v4(),
                            };
                            let fill =
                                simulate_fill(&order, snapshot.mid_price, FillIntent::Close);
                            info!(
                                asset = %fill.asset,
                                side = ?fill.side,
                                size = %fill.size,
                                price = %fill.price,
                                notional_usd = %fill.notional_usd,
                                intent = "close",
                                "simulated fill"
                            );
                            if let Err(e) = perps_executor::append(&fills_path, &fill) {
                                warn!(error = %e, "failed to persist fill");
                            }
                        }
                        None => warn!(
                            asset = %asset,
                            "strategy emitted Close but portfolio had no position"
                        ),
                    }
                }
                Decision::Hold { .. } => {}
            }

            let record = DecisionRecord::new(signal.apy, decision);
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

fn log_decision(signal: &FundingSignal, decision: &Decision) {
    match decision {
        Decision::Open {
            asset,
            side,
            notional_usd,
        } => info!(
            asset = %asset,
            apy = %signal.apy,
            side = ?side,
            notional_usd = %notional_usd,
            kind = "open",
            "dry-run decision"
        ),
        Decision::Close { asset } => info!(
            asset = %asset,
            apy = %signal.apy,
            kind = "close",
            "dry-run decision"
        ),
        Decision::Hold { asset } => info!(
            asset = %asset,
            apy = %signal.apy,
            kind = "hold",
            "dry-run decision"
        ),
    }
}
