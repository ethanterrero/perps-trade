use crate::VenueClient;
use async_trait::async_trait;
use chrono::Utc;
use perps_types::{FundingRate, Order, Position, Venue, VenueError};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;

/// Hyperliquid REST client. Read-only for now — no signing wired.
/// Phase 1 only exercises public endpoints (meta, assetCtxs).
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

    async fn post_info<T: for<'de> Deserialize<'de>>(
        &self,
        body: serde_json::Value,
    ) -> Result<T, VenueError> {
        let url = format!("{}/info", self.api_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| VenueError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(VenueError::Other(format!(
                "info endpoint returned {status}: {body}"
            )));
        }
        resp.json::<T>()
            .await
            .map_err(|e| VenueError::Other(format!("decode failed: {e}")))
    }
}

#[derive(Debug, Deserialize)]
struct UniverseEntry {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Meta {
    universe: Vec<UniverseEntry>,
}

#[derive(Debug, Deserialize)]
struct AssetCtx {
    funding: String,
}

fn extract_funding(
    meta: &Meta,
    ctxs: &[AssetCtx],
    asset: &str,
) -> Result<Decimal, VenueError> {
    let idx = meta
        .universe
        .iter()
        .position(|u| u.name.eq_ignore_ascii_case(asset))
        .ok_or_else(|| VenueError::UnknownAsset(asset.to_string()))?;
    let ctx = ctxs.get(idx).ok_or_else(|| {
        VenueError::Other(format!("asset ctxs missing index {idx} for {asset}"))
    })?;
    Decimal::from_str(&ctx.funding)
        .map_err(|e| VenueError::Other(format!("bad funding decimal {:?}: {e}", ctx.funding)))
}

#[async_trait]
impl VenueClient for HyperliquidClient {
    async fn funding_rate(&self, asset: &str) -> Result<FundingRate, VenueError> {
        let body = json!({"type": "metaAndAssetCtxs"});
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = self.post_info(body).await?;
        let rate = extract_funding(&meta, &ctxs, asset)?;
        Ok(FundingRate {
            venue: Venue::Hyperliquid,
            asset: asset.to_string(),
            rate,
            interval_hours: 1,
            observed_at: Utc::now(),
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    const FIXTURE: &str = r#"[
        {"universe": [
            {"name": "BTC", "szDecimals": 5},
            {"name": "ETH", "szDecimals": 4},
            {"name": "SOL", "szDecimals": 2}
        ]},
        [
            {"funding": "0.0000125", "openInterest": "1", "markPx": "100"},
            {"funding": "-0.0000050", "openInterest": "1", "markPx": "100"},
            {"funding": "0.0001000", "openInterest": "1", "markPx": "100"}
        ]
    ]"#;

    #[test]
    fn parses_funding_from_meta_and_ctxs() {
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(extract_funding(&meta, &ctxs, "BTC").unwrap(), dec!(0.0000125));
        assert_eq!(extract_funding(&meta, &ctxs, "eth").unwrap(), dec!(-0.0000050));
        assert_eq!(extract_funding(&meta, &ctxs, "SOL").unwrap(), dec!(0.0001));
    }

    #[test]
    fn unknown_asset_errors() {
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = serde_json::from_str(FIXTURE).unwrap();
        assert!(matches!(
            extract_funding(&meta, &ctxs, "DOGE"),
            Err(VenueError::UnknownAsset(_))
        ));
    }
}
