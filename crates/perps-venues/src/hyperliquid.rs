use crate::VenueClient;
use async_trait::async_trait;
use perps_types::{FundingRate, Order, Position, VenueError};
use reqwest::Client;

/// Hyperliquid REST client. Read-only for now — no signing wired.
/// Phase 1 only exercises public endpoints (meta, fundingHistory).
#[allow(dead_code)]
pub struct HyperliquidClient {
    http: Client,
    api_url: String,
}

impl HyperliquidClient {
    pub fn new(api_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            api_url: api_url.into(),
        }
    }
}

#[async_trait]
impl VenueClient for HyperliquidClient {
    async fn funding_rate(&self, _asset: &str) -> Result<FundingRate, VenueError> {
        // TODO(phase-1): POST {api_url}/info with {"type": "metaAndAssetCtxs"}, parse funding.
        Err(VenueError::Other("not implemented".into()))
    }

    async fn positions(&self) -> Result<Vec<Position>, VenueError> {
        // TODO(phase-2): POST /info with {"type": "clearinghouseState", "user": ...}.
        Err(VenueError::Other("not implemented".into()))
    }

    async fn place_order(&self, _order: Order) -> Result<String, VenueError> {
        // TODO(phase-2): EIP-712 sign + POST /exchange.
        Err(VenueError::Other("not implemented".into()))
    }

    async fn cancel_order(&self, _asset: &str, _order_id: &str) -> Result<(), VenueError> {
        // TODO(phase-2).
        Err(VenueError::Other("not implemented".into()))
    }
}
