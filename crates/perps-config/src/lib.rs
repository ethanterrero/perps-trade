use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub venue: VenueSettings,
    pub risk: RiskSettings,
    pub strategy: StrategySettings,
    pub telemetry: TelemetrySettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct VenueSettings {
    pub hyperliquid: HyperliquidSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct HyperliquidSettings {
    pub api_url: String,
    pub ws_url: String,
    pub testnet: bool,
    /// When `true`, never place real orders — the bot simulates fills locally.
    /// Defaults to `true` if unset so an in-place upgrade can't accidentally
    /// start live trading. Set to `false` AND pass `--allow-live` to the
    /// `run` command to enable real orders.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
    /// Hex private key (0x-prefixed, no quotes) used for EIP-712 order signing.
    /// Required when `dry_run = false`. Never commit a real key — use an env
    /// override or the macOS keychain loader (PR #14).
    #[serde(default)]
    pub secret_key: Option<String>,
    /// 0x address of the trading account. Optional today; required once
    /// `clearinghouseState` queries are wired (reconciliation).
    #[serde(default)]
    pub account_address: Option<String>,
}

fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct RiskSettings {
    pub max_position_usd: Decimal,
    pub liq_buffer_pct: Decimal,
    pub max_leverage: Decimal,
}

#[derive(Debug, Deserialize, Clone)]
pub struct StrategySettings {
    pub assets: Vec<String>,
    pub min_funding_apy_to_enter: Decimal,
    pub max_funding_apy_to_exit: Decimal,
    pub decision_interval_seconds: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelemetrySettings {
    pub log_level: String,
    pub state_dir: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config load: {0}")]
    Load(#[from] config::ConfigError),
    #[error("config dir missing: {0}")]
    Missing(String),
}

/// Load `config/default.toml`, then layer `config/{RUN_ENV}.toml` if present,
/// then env vars prefixed `PERPS_` (double underscore = path separator,
/// e.g. `PERPS_RISK__MAX_POSITION_USD=500`).
pub fn load(config_dir: impl AsRef<Path>) -> Result<Settings, ConfigError> {
    let _ = dotenvy::dotenv();

    let dir = config_dir.as_ref();
    if !dir.is_dir() {
        return Err(ConfigError::Missing(dir.display().to_string()));
    }

    let mut builder =
        config::Config::builder().add_source(config::File::from(dir.join("default.toml")));

    if let Ok(env) = std::env::var("RUN_ENV") {
        let path = dir.join(format!("{env}.toml"));
        if path.exists() {
            builder = builder.add_source(config::File::from(path));
        }
    }

    builder = builder.add_source(
        config::Environment::with_prefix("PERPS")
            .prefix_separator("_")
            .separator("__")
            .try_parsing(true),
    );

    Ok(builder.build()?.try_deserialize()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_defaults_to_true_when_absent() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("default.toml"),
            r#"
[venue.hyperliquid]
api_url = "https://api.hyperliquid-testnet.xyz"
ws_url = "wss://api.hyperliquid-testnet.xyz/ws"
testnet = true

[risk]
max_position_usd = 1000
liq_buffer_pct = 0.30
max_leverage = 3

[strategy]
assets = ["BTC"]
min_funding_apy_to_enter = 0.10
max_funding_apy_to_exit = 0.02
decision_interval_seconds = 300

[telemetry]
log_level = "info"
state_dir = "state"
"#,
        )
        .unwrap();
        let s = load(dir.path()).unwrap();
        // dry_run absent in toml -> default true. Existing deployments don't
        // get surprised by an unset field flipping to live.
        assert!(s.venue.hyperliquid.dry_run);
        assert!(s.venue.hyperliquid.secret_key.is_none());
    }
}
