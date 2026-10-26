//! `perps-bot backtest`: pull funding history from the venue and replay it
//! through the strategy with `perps-backtest`. Pure presentation plus I/O —
//! the replay math lives in `perps_backtest::run`.
//!
//! History comes from Hyperliquid **mainnet** by default, whatever the config's
//! `api_url` says: testnet funding isn't real market data. Only the public
//! `/info` endpoint is used, so no keys are involved.
//!
//! Each asset's history is fetched once and replayed under several rules so
//! the configured one has something to be judged against:
//!   - `smoothed E/X`: enter ≥ E% / exit < X% on the trailing window, once per
//!     `--enter` × `--exit` combination (default: the configured thresholds).
//!   - `raw hourly`: the first combination on the latest hourly print (window 1).
//!   - `always on`: enter once the window fills, never exit.

use chrono::{Duration, Utc};
use clap::Parser;
use perps_backtest::{BacktestParams, BacktestReport};
use perps_strategy::Thresholds;
use perps_venues::hyperliquid::HyperliquidClient;
use perps_venues::VenueClient;
use rust_decimal::Decimal;

#[derive(Parser, Debug)]
pub struct BacktestArgs {
    /// Path to config directory (thresholds, window, fees, notional, assets).
    #[arg(long, default_value = "config")]
    config_dir: String,
    /// Info API to pull funding history from. Defaults to mainnet because
    /// testnet funding isn't real market data.
    #[arg(long, default_value = "https://api.hyperliquid.xyz")]
    api_url: String,
    /// Days of history to replay.
    #[arg(long, default_value_t = 365)]
    days: i64,
    /// Comma-separated assets. Defaults to `strategy.assets` from config.
    #[arg(long, value_delimiter = ',')]
    assets: Vec<String>,
    /// Entry thresholds to try, comma-separated (e.g. 0.05,0.08,0.10).
    /// Defaults to `strategy.min_funding_apy_to_enter`.
    #[arg(long, value_delimiter = ',', allow_negative_numbers = true)]
    enter: Vec<Decimal>,
    /// Exit thresholds to try, comma-separated (e.g. -0.05,0,0.02).
    /// Defaults to `strategy.max_funding_apy_to_exit`.
    #[arg(long, value_delimiter = ',', allow_negative_numbers = true)]
    exit: Vec<Decimal>,
    /// Override `strategy.signal_window_hours`.
    #[arg(long)]
    window_hours: Option<u32>,
    /// Slippage per leg per fill, in basis points.
    #[arg(long, default_value = "1")]
    slippage_bps: Decimal,
}

pub async fn run(args: BacktestArgs) -> anyhow::Result<()> {
    let settings = perps_config::load(&args.config_dir)?;
    let assets = if args.assets.is_empty() {
        settings.strategy.assets.clone()
    } else {
        args.assets.clone()
    };
    let window_hours = args.window_hours.unwrap_or(settings.strategy.signal_window_hours);
    let enters = or_default(&args.enter, settings.strategy.min_funding_apy_to_enter);
    let exits = or_default(&args.exit, settings.strategy.max_funding_apy_to_exit);
    let params = BacktestParams {
        thresholds: Thresholds {
            min_apy_to_enter: enters[0],
            max_apy_to_exit: exits[0],
        },
        window_hours,
        min_signal_hours: window_hours.min(settings.strategy.min_signal_hours.max(1)),
        fees: settings.fees,
        slippage: args.slippage_bps / Decimal::from(10_000),
        notional_usd: settings.risk.max_position_usd,
    };
    let raw_hourly = BacktestParams {
        window_hours: 1,
        min_signal_hours: 1,
        ..params
    };

    let end = Utc::now();
    let start = end - Duration::days(args.days);
    let client = HyperliquidClient::new(&args.api_url);

    println!(
        "backtest: {} funding {} → {} ({}d), ${}/leg notional",
        args.api_url,
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d"),
        args.days,
        params.notional_usd.normalize(),
    );
    println!(
        "smoothed E/X = enter ≥ E% / exit < X% on the {}h mean; fees perp {} spot {} per side; slippage {} bp per leg",
        window_hours,
        pct(params.fees.perp_taker),
        pct(params.fees.spot_taker),
        args.slippage_bps.normalize(),
    );
    println!("price PnL taken as zero (equal-size perp short + spot long); basis not modelled");
    println!();
    println!(
        "{:<6} {:<16} {:>7} {:>7} {:>11} {:>10} {:>10} {:>11} {:>8}",
        "asset", "rule", "entries", "in-pos", "funding", "fees", "slippage", "net", "net/yr"
    );

    for asset in &assets {
        let history = match client.funding_history(asset, start, end).await {
            Ok(h) if !h.is_empty() => h,
            Ok(_) => {
                println!("{asset:<6} no funding history returned");
                continue;
            }
            Err(e) => {
                println!("{asset:<6} history fetch failed: {e}");
                continue;
            }
        };
        for &enter in &enters {
            for &exit in &exits {
                let p = BacktestParams {
                    thresholds: Thresholds {
                        min_apy_to_enter: enter,
                        max_apy_to_exit: exit,
                    },
                    ..params
                };
                let name = format!("smoothed {}/{}", pct_num(enter), pct_num(exit));
                print_row(asset, &name, &perps_backtest::run(asset, &history, &p));
            }
        }
        print_row(asset, "raw hourly", &perps_backtest::run(asset, &history, &raw_hourly));
        print_row(asset, "always on", &perps_backtest::run(asset, &history, &params.always_on()));
    }
    Ok(())
}

fn print_row(asset: &str, rule: &str, r: &BacktestReport) {
    println!(
        "{:<6} {:<16} {:>7} {:>7} {:>11} {:>10} {:>10} {:>11} {:>8}",
        asset,
        rule,
        r.entries,
        pct0(r.time_in_position()),
        usd(r.funding_usd),
        usd(r.fees_usd),
        usd(r.slippage_usd),
        usd(r.net_usd()),
        pct(r.net_apy()),
    );
}

fn or_default(values: &[Decimal], default: Decimal) -> Vec<Decimal> {
    if values.is_empty() {
        vec![default]
    } else {
        values.to_vec()
    }
}

fn pct(d: Decimal) -> String {
    format!("{}%", pct_num(d))
}

/// Percentage number without the sign, to three decimals (0.00045 → 0.045).
fn pct_num(d: Decimal) -> String {
    (d * Decimal::from(100)).round_dp(3).normalize().to_string()
}

fn pct0(d: Decimal) -> String {
    format!("{}%", (d * Decimal::from(100)).round_dp(0).normalize())
}

fn usd(d: Decimal) -> String {
    let rounded = d.round_dp(2);
    if rounded.is_sign_negative() {
        format!("-${}", (-rounded).normalize())
    } else {
        format!("${}", rounded.normalize())
    }
}
