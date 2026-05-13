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
            .separator("__")
            .try_parsing(true),
    );

    Ok(builder.build()?.try_deserialize()?)
}
