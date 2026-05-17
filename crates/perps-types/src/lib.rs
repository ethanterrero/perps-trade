use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Venue {
    Hyperliquid,
    Binance,
    Bybit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn flip(self) -> Self {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingRate {
    pub venue: Venue,
    pub asset: String,
    /// Per-interval rate. e.g. 0.0001 = 0.01% per interval.
    pub rate: Decimal,
    /// Hours between funding payments. 1 on Hyperliquid, 8 on Binance/Bybit.
    pub interval_hours: u8,
    pub observed_at: DateTime<Utc>,
}

impl FundingRate {
    pub fn annualized(&self) -> Decimal {
        let periods_per_year = Decimal::from(24 * 365) / Decimal::from(self.interval_hours);
        self.rate * periods_per_year
    }
}

/// One-stop venue snapshot: funding rate + mid price for an asset at a moment in
/// time. Returned by `VenueClient::market_snapshot` so callers don't have to
/// pay for two HTTP round-trips when the venue's response already contains both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketSnapshot {
    pub funding: FundingRate,
    /// Mid-market price (midpoint between best bid and ask) in USD per base unit.
    pub mid_price: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub venue: Venue,
    pub asset: String,
    pub side: Side,
    /// Base asset units (e.g. BTC, not USD).
    pub size: Decimal,
    pub entry_price: Decimal,
    pub leverage: Decimal,
    pub margin_used: Decimal,
    pub liquidation_price: Option<Decimal>,
}

impl Position {
    pub fn notional_usd(&self, mark_price: Decimal) -> Decimal {
        self.size * mark_price
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub venue: Venue,
    pub asset: String,
    pub side: Side,
    pub size: Decimal,
    /// None = market order.
    pub limit_price: Option<Decimal>,
    pub reduce_only: bool,
    pub client_id: uuid::Uuid,
}

#[derive(Debug, thiserror::Error)]
pub enum VenueError {
    #[error("network error: {0}")]
    Network(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("rejected by venue: {0}")]
    Rejected(String),
    #[error("symbol not found: {0}")]
    UnknownAsset(String),
    #[error("other: {0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn hourly_funding_annualizes() {
        let fr = FundingRate {
            venue: Venue::Hyperliquid,
            asset: "BTC".into(),
            rate: dec!(0.0001),
            interval_hours: 1,
            observed_at: Utc::now(),
        };
        // 0.01% * 24 * 365 = 87.6%
        assert_eq!(fr.annualized(), dec!(0.8760));
    }

    #[test]
    fn side_flips() {
        assert_eq!(Side::Long.flip(), Side::Short);
        assert_eq!(Side::Short.flip(), Side::Long);
    }
}
