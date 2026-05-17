use async_trait::async_trait;
use perps_types::{MarketSnapshot, Order, Position, VenueError};

pub mod hyperliquid;

/// Common surface every venue must implement so the strategy stays venue-agnostic.
/// Implementations live in submodules (`hyperliquid`, `binance`, ...).
#[async_trait]
pub trait VenueClient: Send + Sync {
    /// Funding rate + mid price in a single round-trip. Most venue endpoints
    /// (e.g. Hyperliquid's `metaAndAssetCtxs`) return both alongside each other,
    /// so fetching them together avoids redundant requests during a poll tick.
    async fn market_snapshot(&self, asset: &str) -> Result<MarketSnapshot, VenueError>;
    async fn positions(&self) -> Result<Vec<Position>, VenueError>;
    async fn place_order(&self, order: Order) -> Result<String, VenueError>;
    async fn cancel_order(&self, asset: &str, order_id: &str) -> Result<(), VenueError>;
}
