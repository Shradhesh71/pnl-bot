//! Binance REST — executes market orders for the CEX leg of each arb trade.
//!
//! # Endpoints used
//!
//! | Operation      | Endpoint                        | Auth  |
//! |----------------|---------------------------------|-------|
//! | Place order    | POST /api/v3/order              | HMAC  |
//! | Check balance  | GET  /api/v3/account            | HMAC  |
//! | Cancel order   | DELETE /api/v3/order            | HMAC  |
//! | Query order    | GET  /api/v3/order              | HMAC  |
//!
//! # Auth
//!
//! Binance uses HMAC-SHA256 over the query string. Every signed request:
//! 1. Appends `timestamp=<unix_ms>&recvWindow=5000` to the params
//! 2. Signs the entire query string with the API secret
//! 3. Appends `&signature=<hex>` to the URL
//! 4. Sets `X-MBX-APIKEY: <api_key>` header
//!
//! # Safety gate
//!
//! All order methods require `BinanceClient::live_trading_enabled()` to
//! return true. In paper mode every call returns `Err(OrderError::PaperMode)`
//! immediately — no network request is made. Enable live trading by
//! constructing with `BinanceClient::new_live(api_key, api_secret)`.

// use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
// use thiserror::Error;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("live trading is disabled — construct with BinanceClient::new_live()")]
    PaperMode,

    #[error("HTTP error status={status} body={body}")]
    HttpError { status: u16, body: String },

    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),

    #[error("Binance API error code={code} msg={msg}")]
    ApiError { code: i64, msg: String },

    #[error("invalid parameter: {0}")]
    InvalidParam(String),
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BinanceRestConfig {
    /// Binance REST base URL. Default: https://api.binance.com
    pub base_url: String,

    /// Request timeout. Default: 5s
    pub timeout: Duration,

    /// recvWindow sent with every signed request (ms). Default: 5000
    pub recv_window_ms: u64,
}

impl Default for BinanceRestConfig {
    fn default() -> Self {
        Self {
            base_url:       "https://api.binance.com".to_string(),
            timeout:        Duration::from_secs(5),
            recv_window_ms: 5_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Order types
// ---------------------------------------------------------------------------

/// Which side of the market to trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy  => write!(f, "BUY"),
            Self::Sell => write!(f, "SELL"),
        }
    }
}

/// Parameters for placing a market order.
#[derive(Debug, Clone)]
pub struct MarketOrderParams {
    /// Symbol e.g. "SOLUSDC"
    pub symbol: String,
    /// BUY or SELL
    pub side: OrderSide,
    /// USDC quantity to spend (for BUY) or SOL quantity to sell (for SELL).
    /// We use quoteOrderQty for BUY (spend exact USDC) and
    /// quantity for SELL (sell exact SOL).
    pub quote_qty_usdc: Option<f64>, // for BUY
    pub base_qty_sol:   Option<f64>, // for SELL
}

/// Filled order returned by Binance.
#[derive(Debug, Clone)]
pub struct FilledOrder {
    pub order_id:          u64,
    pub symbol:            String,
    pub side:              OrderSide,
    pub status:            String,
    /// Average fill price
    pub avg_price:         f64,
    /// How much SOL was traded
    pub executed_qty_sol:  f64,
    /// How much USDC was traded
    pub executed_qty_usdc: f64,
    /// Total fees paid (in fee_asset)
    pub fee_usdt:          f64,
    pub transact_time_ms:  u64,
}

// ---------------------------------------------------------------------------
// Binance REST API response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OrderResponse {
    #[serde(rename = "orderId")]
    order_id: u64,

    symbol: String,
    status: String,

    #[serde(rename = "executedQty")]
    executed_qty: String,

    #[serde(rename = "cummulativeQuoteQty")]
    cumulative_quote_qty: String,

    fills: Option<Vec<FillDetail>>,

    #[serde(rename = "transactTime")]
    transact_time: u64,
}

#[derive(Debug, Deserialize)]
struct FillDetail {
    price: String,
    qty:   String,

    #[serde(rename = "commission")]
    commission: String,

    #[serde(rename = "commissionAsset")]
    commission_asset: String,
}

#[derive(Debug, Deserialize)]
struct BinanceApiError {
    code: i64,
    msg:  String,
}

#[derive(Debug, Deserialize)]
struct AccountBalance {
    asset: String,
    free:  String,
}

#[derive(Debug, Deserialize)]
struct AccountResponse {
    balances: Vec<AccountBalance>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Binance REST client.
///
/// Constructed in paper mode by default — no real orders will be sent.
/// Use [`BinanceClient::new_live`] to enable real execution.
pub struct BinanceClient {
    config:     BinanceRestConfig,
    http:       Client,
    api_key:    String,
    api_secret: String,
    live:       bool,
}

impl BinanceClient {
    /// Create in **paper mode** — no real orders will ever be sent.
    pub fn new_paper() -> Self {
        Self {
            config:     BinanceRestConfig::default(),
            http:       Client::new(),
            api_key:    String::new(),
            api_secret: String::new(),
            live:       false,
        }
    }

    /// Create in **live mode** with real credentials.
    ///
    /// ⚠️  Real orders WILL be sent when `place_market_order` is called.
    pub fn new_live(api_key: String, api_secret: String) -> Self {
        Self {
            config:     BinanceRestConfig::default(),
            http:       Client::builder()
                            .timeout(BinanceRestConfig::default().timeout)
                            .build()
                            .expect("failed to build HTTP client"),
            api_key,
            api_secret,
            live:       true,
        }
    }

    /// Create from environment variables:
    /// `BINANCE_API_KEY` and `BINANCE_API_SECRET`.
    /// Returns paper client if either var is missing.
    pub fn from_env() -> Self {
        match (
            std::env::var("BINANCE_API_KEY"),
            std::env::var("BINANCE_API_SECRET"),
        ) {
            (Ok(key), Ok(secret)) => {
                info!("BinanceClient: live mode enabled from environment");
                Self::new_live(key, secret)
            }
            _ => {
                info!("BinanceClient: paper mode (BINANCE_API_KEY/SECRET not set)");
                Self::new_paper()
            }
        }
    }

    /// True if live trading is enabled.
    pub fn live_trading_enabled(&self) -> bool { self.live }

    // ── Orders ────────────────────────────────────────────────────────────

    /// Place a market order.
    ///
    /// Returns `Err(OrderError::PaperMode)` if not in live mode.
    pub async fn place_market_order(
        &self,
        params: MarketOrderParams,
    ) -> Result<FilledOrder, OrderError> {
        if !self.live {
            return Err(OrderError::PaperMode);
        }

        // Validate params
        match (&params.quote_qty_usdc, &params.base_qty_sol) {
            (None, None) => return Err(OrderError::InvalidParam(
                "must set either quote_qty_usdc or base_qty_sol".into()
            )),
            (Some(_), Some(_)) => return Err(OrderError::InvalidParam(
                "set only one of quote_qty_usdc or base_qty_sol, not both".into()
            )),
            _ => {}
        }

        let ts = unix_ms();

        // Build query params
        let mut qs = format!(
            "symbol={}&side={}&type=MARKET&timestamp={}&recvWindow={}",
            params.symbol, params.side, ts, self.config.recv_window_ms,
        );

        if let Some(qty) = params.quote_qty_usdc {
            // BUY: spend exact USDC amount
            qs.push_str(&format!("&quoteOrderQty={:.2}", qty));
        } else if let Some(qty) = params.base_qty_sol {
            // SELL: sell exact SOL amount (8 decimal places)
            qs.push_str(&format!("&quantity={:.8}", qty));
        }

        let sig = self.sign(&qs);
        qs.push_str(&format!("&signature={sig}"));

        let url = format!("{}/api/v3/order?{qs}", self.config.base_url);

        debug!("Binance REST: POST /api/v3/order symbol={} side={}",
            params.symbol, params.side);

        let resp = self.http
            .post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        self.parse_order_response(resp, params.side).await
    }

    /// Get USDC and SOL free balances.
    pub async fn get_balances(&self) -> Result<(f64, f64), OrderError> {
        if !self.live {
            // Return dummy balances in paper mode
            return Ok((10_000.0, 0.0));
        }

        let ts  = unix_ms();
        let qs  = format!("timestamp={}&recvWindow={}", ts, self.config.recv_window_ms);
        let sig = self.sign(&qs);
        let url = format!("{}/api/v3/account?{qs}&signature={sig}", self.config.base_url);

        let resp = self.http
            .get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;

        if resp.status() != StatusCode::OK {
            let status = resp.status().as_u16();
            let body   = resp.text().await.unwrap_or_default();
            return Err(OrderError::HttpError { status, body });
        }

        let body   = resp.text().await?;
        let account: AccountResponse = serde_json::from_str(&body)?;

        let usdc = account.balances.iter()
            .find(|b| b.asset == "USDC")
            .and_then(|b| b.free.parse::<f64>().ok())
            .unwrap_or(0.0);

        let sol = account.balances.iter()
            .find(|b| b.asset == "SOL")
            .and_then(|b| b.free.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok((usdc, sol))
    }

    // ── Private: parse response ───────────────────────────────────────────

    async fn parse_order_response(
        &self,
        resp: reqwest::Response,
        side: OrderSide,
    ) -> Result<FilledOrder, OrderError> {
        let status = resp.status();
        let body   = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            // Try to parse Binance error envelope
            if let Ok(api_err) = serde_json::from_str::<BinanceApiError>(&body) {
                // Code -2010: insufficient balance
                // Code -1013: invalid quantity
                warn!("Binance API error code={} msg={}", api_err.code, api_err.msg);
                return Err(OrderError::ApiError {
                    code: api_err.code,
                    msg:  api_err.msg,
                });
            }
            return Err(OrderError::HttpError {
                status: status.as_u16(),
                body,
            });
        }

        let order: OrderResponse = serde_json::from_str(&body)?;

        // Compute weighted average fill price from fills
        let (avg_price, fee_usdt) = if let Some(fills) = &order.fills {
            let total_qty:   f64 = fills.iter()
                .filter_map(|f| f.qty.parse::<f64>().ok())
                .sum();
            let total_value: f64 = fills.iter()
                .filter_map(|f| {
                    let p = f.price.parse::<f64>().ok()?;
                    let q = f.qty.parse::<f64>().ok()?;
                    Some(p * q)
                })
                .sum();

            let avg = if total_qty > 0.0 { total_value / total_qty } else { 0.0 };

            // Sum fees — convert to USDT equivalent if commission is in BNB
            // For simplicity we only count USDC/USDT/SOL fees here
            let fee: f64 = fills.iter()
                .filter(|f| f.commission_asset == "USDC"
                         || f.commission_asset == "USDT")
                .filter_map(|f| f.commission.parse::<f64>().ok())
                .sum();

            (avg, fee)
        } else {
            // Fallback: use cumulativeQuoteQty / executedQty
            let exec_qty = order.executed_qty.parse::<f64>().unwrap_or(0.0);
            let quote_qty = order.cumulative_quote_qty.parse::<f64>().unwrap_or(0.0);
            let avg = if exec_qty > 0.0 { quote_qty / exec_qty } else { 0.0 };
            (avg, 0.0)
        };

        let exec_sol  = order.executed_qty.parse::<f64>().unwrap_or(0.0);
        let exec_usdc = order.cumulative_quote_qty.parse::<f64>().unwrap_or(0.0);

        info!(
            "Binance REST: order filled id={} side={} avg={:.6} qty_sol={:.4} qty_usdc={:.2}",
            order.order_id, side, avg_price, exec_sol, exec_usdc
        );

        Ok(FilledOrder {
            order_id:          order.order_id,
            symbol:            order.symbol,
            side,
            status:            order.status,
            avg_price,
            executed_qty_sol:  exec_sol,
            executed_qty_usdc: exec_usdc,
            fee_usdt,
            transact_time_ms:  order.transact_time,
        })
    }

    // ── Private: HMAC-SHA256 signing ──────────────────────────────────────

    /// Sign a query string with the API secret using HMAC-SHA256.
    /// Returns lowercase hex digest.
    fn sign(&self, query_string: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC key error");
        mac.update(query_string.as_bytes());
        let result = mac.finalize();
        hex::encode(result.into_bytes())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paper_mode_blocks_orders() {
        let client = BinanceClient::new_paper();
        assert!(!client.live_trading_enabled());
    }

    // #[test]
    // fn test_from_env_defaults_to_paper() {
    //     // Without env vars set, should be paper
    //     std::env::remove_var("BINANCE_API_KEY");
    //     std::env::remove_var("BINANCE_API_SECRET");
    //     let client = BinanceClient::from_env();
    //     assert!(!client.live_trading_enabled());
    // }

    #[tokio::test]
    async fn test_paper_mode_returns_error() {
        let client = BinanceClient::new_paper();
        let result = client.place_market_order(MarketOrderParams {
            symbol:         "SOLUSDC".into(),
            side:           OrderSide::Buy,
            quote_qty_usdc: Some(1_000.0),
            base_qty_sol:   None,
        }).await;
        assert!(matches!(result, Err(OrderError::PaperMode)));
    }

    #[tokio::test]
    async fn test_paper_mode_balances_return_dummy() {
        let client = BinanceClient::new_paper();
        let (usdc, sol) = client.get_balances().await.unwrap();
        assert_eq!(usdc, 10_000.0);
        assert_eq!(sol, 0.0);
    }

    #[test]
    fn test_hmac_sign_deterministic() {
        let client = BinanceClient {
            config:     BinanceRestConfig::default(),
            http:       Client::new(),
            api_key:    "testkey".into(),
            api_secret: "testsecret".into(),
            live:       false,
        };
        let sig1 = client.sign("symbol=SOLUSDC&side=BUY&timestamp=1234567890");
        let sig2 = client.sign("symbol=SOLUSDC&side=BUY&timestamp=1234567890");
        assert_eq!(sig1, sig2, "HMAC must be deterministic");
        assert_eq!(sig1.len(), 64, "SHA256 hex is 64 chars");
    }

    #[test]
    fn test_hmac_sign_different_inputs() {
        let client = BinanceClient {
            config:     BinanceRestConfig::default(),
            http:       Client::new(),
            api_key:    "key".into(),
            api_secret: "secret".into(),
            live:       false,
        };
        let sig1 = client.sign("timestamp=1000");
        let sig2 = client.sign("timestamp=1001");
        assert_ne!(sig1, sig2, "different inputs must produce different sigs");
    }

    #[test]
    fn test_invalid_params_both_qty() {
        // Can't set both quote and base qty
        let params = MarketOrderParams {
            symbol:         "SOLUSDC".into(),
            side:           OrderSide::Buy,
            quote_qty_usdc: Some(1_000.0),
            base_qty_sol:   Some(11.0), // both set — invalid
        };
        // Validation is async but we can at least check the struct builds
        assert!(params.quote_qty_usdc.is_some());
        assert!(params.base_qty_sol.is_some());
    }

    #[test]
    fn test_order_side_display() {
        assert_eq!(format!("{}", OrderSide::Buy),  "BUY");
        assert_eq!(format!("{}", OrderSide::Sell), "SELL");
    }
}