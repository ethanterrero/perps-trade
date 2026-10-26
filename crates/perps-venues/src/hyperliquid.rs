use crate::VenueClient;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use std::collections::HashMap;
use hyperliquid_rust_sdk::{
    BaseUrl, ExchangeClient, ExchangeResponseStatus, MarketCloseParams, MarketOrderParams,
};
use perps_types::{
    FundingRate, Instrument, MarketSnapshot, Order, Position, Side, Venue, VenueError,
};
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
    /// Perp asset (uppercase) → spot token quoted against USDC for the hedge
    /// leg, e.g. `BTC` → `UBTC`. Unmapped assets get no spot mid.
    spot_tokens: HashMap<String, String>,
}

/// Hyperliquid returns at most this many funding rows per `fundingHistory` call.
const FUNDING_HISTORY_PAGE: usize = 500;

/// Retries for a rate-limited info request, with the backoff doubling from 1s.
/// A year of history is ~18 pages per asset, which trips the public rate limit
/// when several assets are pulled back to back.
const RATE_LIMIT_RETRIES: u32 = 5;

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
            spot_tokens: HashMap::new(),
        }
    }

    /// Map perp assets to the spot tokens used for the hedge leg so
    /// `market_snapshot` also returns a spot mid. Keys are case-insensitive.
    pub fn with_spot_tokens(mut self, spot_tokens: &HashMap<String, String>) -> Self {
        self.spot_tokens = spot_tokens
            .iter()
            .map(|(asset, token)| (asset.to_ascii_uppercase(), token.clone()))
            .collect();
        self
    }

    async fn spot_mid(&self, token: &str) -> Result<Decimal, VenueError> {
        let body = json!({"type": "spotMetaAndAssetCtxs"});
        let (meta, ctxs): (SpotMeta, Vec<SpotAssetCtx>) = self.post_info(body).await?;
        extract_spot_mid(&meta, &ctxs, token)
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
            spot_tokens: HashMap::new(),
        })
    }

    /// `post_info`, retrying with exponential backoff when rate limited.
    async fn post_info_with_retry<T: for<'de> Deserialize<'de>>(
        &self,
        body: serde_json::Value,
    ) -> Result<T, VenueError> {
        let mut delay = std::time::Duration::from_secs(1);
        let mut attempt = 0;
        loop {
            match self.post_info(body.clone()).await {
                Err(VenueError::RateLimited(msg)) if attempt < RATE_LIMIT_RETRIES => {
                    attempt += 1;
                    tracing::warn!(attempt, delay_secs = delay.as_secs(), %msg, "rate limited, backing off");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                other => return other,
            }
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
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(VenueError::RateLimited(format!("info endpoint returned {status}")));
        }
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

#[derive(Debug, Deserialize)]
struct SpotToken {
    name: String,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct SpotPair {
    /// Pair id used as `coin` in asset ctxs, e.g. `@142`.
    name: String,
    /// `[base_token_index, quote_token_index]`.
    tokens: [usize; 2],
}

#[derive(Debug, Deserialize)]
struct SpotMeta {
    tokens: Vec<SpotToken>,
    universe: Vec<SpotPair>,
}

#[derive(Debug, Deserialize)]
struct SpotAssetCtx {
    coin: String,
    #[serde(rename = "midPx")]
    mid_px: Option<String>,
}

/// Mid of `token`/USDC. Spot ctxs aren't index-aligned with the pair list,
/// so they're matched by pair name (`coin`).
fn extract_spot_mid(
    meta: &SpotMeta,
    ctxs: &[SpotAssetCtx],
    token: &str,
) -> Result<Decimal, VenueError> {
    let token_index = |name: &str| {
        meta.tokens
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .map(|t| t.index)
    };
    let base = token_index(token).ok_or_else(|| VenueError::UnknownAsset(token.to_string()))?;
    let usdc = token_index("USDC")
        .ok_or_else(|| VenueError::Other("spot meta has no USDC token".into()))?;
    let pair = meta
        .universe
        .iter()
        .find(|p| p.tokens == [base, usdc])
        .ok_or_else(|| VenueError::UnknownAsset(format!("{token}/USDC")))?;
    let raw = ctxs
        .iter()
        .find(|c| c.coin == pair.name)
        .and_then(|c| c.mid_px.as_deref())
        .ok_or_else(|| VenueError::Other(format!("spot midPx missing for {token}/USDC")))?;
    Decimal::from_str(raw)
        .map_err(|e| VenueError::Other(format!("bad spot midPx decimal {raw:?}: {e}")))
}

#[derive(Debug, Deserialize)]
struct FundingHistoryRow {
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    time: i64,
}

fn parse_funding_history(asset: &str, rows: &[FundingHistoryRow]) -> Result<Vec<FundingRate>, VenueError> {
    rows.iter()
        .map(|row| {
            let rate = Decimal::from_str(&row.funding_rate).map_err(|e| {
                VenueError::Other(format!("bad fundingRate {:?}: {e}", row.funding_rate))
            })?;
            let observed_at = Utc
                .timestamp_millis_opt(row.time)
                .single()
                .ok_or_else(|| VenueError::Other(format!("bad funding time {}", row.time)))?;
            Ok(FundingRate {
                venue: Venue::Hyperliquid,
                asset: asset.to_string(),
                rate,
                interval_hours: 1,
                observed_at,
            })
        })
        .collect()
}

#[async_trait]
impl VenueClient for HyperliquidClient {
    async fn market_snapshot(&self, asset: &str) -> Result<MarketSnapshot, VenueError> {
        let body = json!({"type": "metaAndAssetCtxs"});
        let (meta, ctxs): (Meta, Vec<AssetCtx>) = self.post_info(body).await?;
        let rate = extract_funding(&meta, &ctxs, asset)?;
        let mid_price = extract_mid_price(&meta, &ctxs, asset)?;
        // A missing spot quote isn't fatal for the snapshot: funding is still
        // worth logging, and the bot refuses to open without a hedge price.
        let spot_mid_price = match self.spot_tokens.get(&asset.to_ascii_uppercase()) {
            Some(token) => match self.spot_mid(token).await {
                Ok(mid) => Some(mid),
                Err(e) => {
                    tracing::warn!(asset = %asset, token = %token, error = %e, "spot mid unavailable");
                    None
                }
            },
            None => None,
        };
        Ok(MarketSnapshot {
            funding: FundingRate {
                venue: Venue::Hyperliquid,
                asset: asset.to_string(),
                rate,
                interval_hours: 1,
                observed_at: Utc::now(),
            },
            mid_price,
            spot_mid_price,
        })
    }

    async fn funding_history(
        &self,
        asset: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<FundingRate>, VenueError> {
        let end_ms = end.timestamp_millis();
        let mut cursor = start.timestamp_millis();
        let mut out = Vec::new();
        while cursor <= end_ms {
            let body = json!({
                "type": "fundingHistory",
                "coin": asset,
                "startTime": cursor,
                "endTime": end_ms,
            });
            let rows: Vec<FundingHistoryRow> = self.post_info_with_retry(body).await?;
            let Some(last) = rows.last() else { break };
            cursor = last.time + 1;
            out.extend(parse_funding_history(asset, &rows)?);
            if rows.len() < FUNDING_HISTORY_PAGE {
                break;
            }
        }
        Ok(out)
    }

    async fn positions(&self) -> Result<Vec<Position>, VenueError> {
        // TODO(phase-2): POST /info with {"type": "clearinghouseState", "user": ...}.
        Err(VenueError::Other("not implemented".into()))
    }

    async fn place_order(&self, order: Order) -> Result<String, VenueError> {
        let sdk = self.sdk.as_ref().ok_or_else(|| {
            VenueError::Auth("no signing wallet configured — call with_signer()".into())
        })?;
        // The SDK calls below address perps by name. A spot order sent through
        // them would trade the perp instead — refuse until spot routing exists.
        if order.instrument == Instrument::Spot {
            return Err(VenueError::Rejected(
                "spot orders are not wired for live trading yet".into(),
            ));
        }

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

    const SPOT_FIXTURE: &str = r#"[
        {"tokens": [
            {"name": "USDC", "index": 0},
            {"name": "HYPE", "index": 150},
            {"name": "UBTC", "index": 197}
        ],
        "universe": [
            {"tokens": [150, 0], "name": "@107", "index": 107},
            {"tokens": [197, 0], "name": "@142", "index": 142}
        ]},
        [
            {"coin": "@142", "midPx": "80491.5"},
            {"coin": "@107", "midPx": "78.39"}
        ]
    ]"#;

    #[test]
    fn spot_mid_matches_pair_by_coin_not_position() {
        // ctxs are deliberately in a different order than the pair list.
        let (meta, ctxs): (SpotMeta, Vec<SpotAssetCtx>) = serde_json::from_str(SPOT_FIXTURE).unwrap();
        assert_eq!(extract_spot_mid(&meta, &ctxs, "UBTC").unwrap(), dec!(80491.5));
        assert_eq!(extract_spot_mid(&meta, &ctxs, "hype").unwrap(), dec!(78.39));
    }

    #[test]
    fn spot_mid_unknown_token_errors() {
        let (meta, ctxs): (SpotMeta, Vec<SpotAssetCtx>) = serde_json::from_str(SPOT_FIXTURE).unwrap();
        assert!(matches!(
            extract_spot_mid(&meta, &ctxs, "UETH"),
            Err(VenueError::UnknownAsset(_))
        ));
    }

    #[test]
    fn parses_funding_history_rows() {
        let rows: Vec<FundingHistoryRow> = serde_json::from_str(
            r#"[{"coin":"BTC","fundingRate":"0.0000125","premium":"-0.00028","time":1793023200010}]"#,
        )
        .unwrap();
        let parsed = parse_funding_history("BTC", &rows).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].rate, dec!(0.0000125));
        assert_eq!(parsed[0].interval_hours, 1);
        assert_eq!(parsed[0].observed_at.timestamp_millis(), 1793023200010);
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
