use crate::VenueClient;
use async_trait::async_trait;
use chrono::Utc;
use hyperliquid_rust_sdk::{
    BaseUrl, ExchangeClient, ExchangeResponseStatus, MarketCloseParams, MarketOrderParams,
};
use perps_types::{FundingRate, MarketSnapshot, Order, Position, Side, Venue, VenueError};
use reqwest::Client;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::json;
use std::str::FromStr;

/// Hyperliquid client. Public endpoints (reads) use our hand-rolled reqwest
/// path; private endpoints (signed writes) delegate to `hyperliquid_rust_sdk`.
/// The signer (`sdk`) is `None` until `with_signer()` is called; without it
/// `place_order` / `cancel_order` return `VenueError::Auth`.
pub struct HyperliquidClient {
    http: Client,
    api_url: String,
    sdk: Option<ExchangeClient>,
}

impl HyperliquidClient {
    /// Read-only client. Cannot place orders.
    pub fn new(api_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        Self {
            http,
            api_url: api_url.into(),
            sdk: None,
        }
    }

    /// Client with a signing wallet loaded. The SDK handles EIP-712 signing
    /// and routes signed orders to `/exchange`. `secret_key` is an 0x-prefixed
    /// hex private key — never log it.
    pub async fn with_signer(
        api_url: impl Into<String>,
        secret_key: &str,
        testnet: bool,
    ) -> Result<Self, VenueError> {
        use ethers::signers::LocalWallet;

        let wallet: LocalWallet = secret_key
            .parse()
            .map_err(|e: ethers::signers::WalletError| {
                VenueError::Auth(format!("parse secret key: {e}"))
            })?;
        let base_url = if testnet {
            BaseUrl::Testnet
        } else {
            BaseUrl::Mainnet
        };
        let sdk = ExchangeClient::new(None, wallet, Some(base_url), None, None)
            .await
            .map_err(|e| VenueError::Auth(format!("sdk init: {e}")))?;

        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        Ok(Self {
            http,
            api_url: api_url.into(),
            sdk: Some(sdk),
        })
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
    #[serde(rename = "midPx")]
    mid_px: Option<String>,
}

fn ctx_for<'a>(
    meta: &Meta,
    ctxs: &'a [AssetCtx],
    asset: &str,
) -> Result<&'a AssetCtx, VenueError> {
    let idx = meta
        .universe
        .iter()
        .position(|u| u.name.eq_ignore_ascii_case(asset))
        .ok_or_else(|| VenueError::UnknownAsset(asset.to_string()))?;
    ctxs.get(idx).ok_or_else(|| {
        VenueError::Other(format!("asset ctxs missing index {idx} for {asset}"))
    })
}

fn extract_funding(
    meta: &Meta,
    ctxs: &[AssetCtx],
    asset: &str,
) -> Result<Decimal, VenueError> {
    let ctx = ctx_for(meta, ctxs, asset)?;
    Decimal::from_str(&ctx.funding)
        .map_err(|e| VenueError::Other(format!("bad funding decimal {:?}: {e}", ctx.funding)))
}

fn extract_mid_price(
    meta: &Meta,
    ctxs: &[AssetCtx],
    asset: &str,
) -> Result<Decimal, VenueError> {
    let ctx = ctx_for(meta, ctxs, asset)?;
    let raw = ctx
        .mid_px
        .as_deref()
        .ok_or_else(|| VenueError::Other(format!("midPx missing for {asset}")))?;
    Decimal::from_str(raw)
        .map_err(|e| VenueError::Other(format!("bad midPx decimal {:?}: {e}", raw)))
}

#[async_trait]
impl VenueClient for HyperliquidClient {
    async fn market_snapshot(&self, asset: &str) -> Result<MarketSnapshot, VenueError> {
        let body = json!({"type": "metaAndAssetCtxs"});
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = self.post_info(body).await?;
        let rate = extract_funding(&meta, &ctxs, asset)?;
        let mid_price = extract_mid_price(&meta, &ctxs, asset)?;
        Ok(MarketSnapshot {
            funding: FundingRate {
                venue: Venue::Hyperliquid,
                asset: asset.to_string(),
                rate,
                interval_hours: 1,
                observed_at: Utc::now(),
            },
            mid_price,
        })
    }

    async fn positions(&self) -> Result<Vec<Position>, VenueError> {
        // TODO(phase-2): POST /info with {"type": "clearinghouseState", "user": ...}.
        Err(VenueError::Other("not implemented".into()))
    }

    async fn place_order(&self, order: Order) -> Result<String, VenueError> {
        let sdk = self.sdk.as_ref().ok_or_else(|| {
            VenueError::Auth("no signing wallet configured — call with_signer()".into())
        })?;

        let sz = order
            .size
            .to_f64()
            .ok_or_else(|| VenueError::Other(format!("size {} not f64-representable", order.size)))?;

        let response = if order.reduce_only {
            sdk.market_close(MarketCloseParams {
                asset: &order.asset,
                sz: Some(sz),
                px: None,
                slippage: None,
                cloid: Some(order.client_id),
                wallet: None,
            })
            .await
        } else {
            let is_buy = matches!(order.side, Side::Long);
            sdk.market_open(MarketOrderParams {
                asset: &order.asset,
                is_buy,
                sz,
                px: None,
                slippage: None,
                cloid: Some(order.client_id),
                wallet: None,
            })
            .await
        }
        .map_err(|e| VenueError::Other(format!("sdk order: {e}")))?;

        match response {
            ExchangeResponseStatus::Ok(_) => Ok(order.client_id.to_string()),
            ExchangeResponseStatus::Err(msg) => Err(VenueError::Rejected(msg)),
        }
    }

    async fn cancel_order(&self, asset: &str, order_id: &str) -> Result<(), VenueError> {
        let _sdk = self.sdk.as_ref().ok_or_else(|| {
            VenueError::Auth("no signing wallet configured — call with_signer()".into())
        })?;
        // The SDK's `cancel_by_cloid` wants a Uuid (our client_id). The bot's
        // current order tracking passes the cloid back as a string; parse it.
        let _cloid = uuid::Uuid::parse_str(order_id)
            .map_err(|e| VenueError::Other(format!("bad cloid {order_id:?}: {e}")))?;
        // Wiring `cancel_by_cloid` requires the asset's `oid` resolved via
        // info endpoint — not needed until we track resting orders. Phase 2/3
        // strategy only uses IOC market orders, which don't rest.
        let _ = asset;
        Err(VenueError::Other(
            "cancel_order: resting-order tracking not wired yet (Phase 3+)".into(),
        ))
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
            {"funding": "0.0000125", "openInterest": "1", "markPx": "60000", "midPx": "59999.5"},
            {"funding": "-0.0000050", "openInterest": "1", "markPx": "3000", "midPx": "3000.1"},
            {"funding": "0.0001000", "openInterest": "1", "markPx": "100", "midPx": null}
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

    #[test]
    fn parses_mid_price_when_present() {
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(extract_mid_price(&meta, &ctxs, "BTC").unwrap(), dec!(59999.5));
        assert_eq!(extract_mid_price(&meta, &ctxs, "ETH").unwrap(), dec!(3000.1));
    }

    #[test]
    fn missing_mid_px_errors_clearly() {
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = serde_json::from_str(FIXTURE).unwrap();
        match extract_mid_price(&meta, &ctxs, "SOL") {
            Err(VenueError::Other(msg)) => assert!(msg.contains("midPx missing")),
            other => panic!("expected Other(midPx missing), got {other:?}"),
        }
    }
}
