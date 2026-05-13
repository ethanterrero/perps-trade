use async_trait::async_trait;
use perps_types::{FundingRate, Order, Position, VenueError};

pub mod hyperliquid;

/// Common surface every venue must implement so the strategy stays venue-agnostic.
/// Implementations live in submodules (`hyperliquid`, `binance`, ...).
#[async_trait]
pub trait VenueClient: Send + Sync {
    async fn funding_rate(&self, asset: &str) -> Result<FundingRate, VenueError>;
    async fn positions(&self) -> Result<Vec<Position>, VenueError>;
    async fn place_order(&self, order: Order) -> Result<String, VenueError>;
    async fn cancel_order(&self, asset: &str, order_id: &str) -> Result<(), VenueError>;
}
