use async_trait::async_trait;
use chrono::{DateTime, Utc};
use perps_types::{FundingRate, MarketSnapshot, Order, Position, VenueError};

pub mod hyperliquid;

/// Common surface every venue must implement so the strategy stays venue-agnostic.
/// Implementations live in submodules (`hyperliquid`, `binance`, ...).
#[async_trait]
pub trait VenueClient: Send + Sync {
    /// Funding rate + mid price in a single round-trip. Most venue endpoints
    /// (e.g. Hyperliquid's `metaAndAssetCtxs`) return both alongside each other,
    /// so fetching them together avoids redundant requests during a poll tick.
    async fn market_snapshot(&self, asset: &str) -> Result<MarketSnapshot, VenueError>;
    /// Settled funding payments for `asset` in `[start, end]`, oldest first.
    /// Used to seed the trailing funding window at startup and by the backtest.
    async fn funding_history(
        &self,
        asset: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<FundingRate>, VenueError>;
    async fn positions(&self) -> Result<Vec<Position>, VenueError>;
    async fn place_order(&self, order: Order) -> Result<String, VenueError>;
    async fn cancel_order(&self, asset: &str, order_id: &str) -> Result<(), VenueError>;
}
