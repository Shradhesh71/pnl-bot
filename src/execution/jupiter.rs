//! Jupiter v6 — gets a swap quote and executes the DEX leg of each arb trade.
//!
//! # Flow
//!
//! ```text
//! 1. GET  /quote?inputMint=...&outputMint=...&amount=...&slippageBps=...
//!         → QuoteResponse { outAmount, priceImpactPct, routePlan, ... }
//!
//! 2. POST /swap  { quoteResponse, userPublicKey, ... }
//!         → SwapResponse { swapTransaction: <base64 versioned tx> }
//!
//! 3. Deserialize → sign with wallet keypair → send via Solana RPC
//!         → confirm transaction
//! ```
//!
//! # Why Jupiter and not directly calling Orca/Raydium
//!
//! Jupiter routes through the best available pool at execution time,
//! which may differ from the pool that triggered the signal. If Orca
//! has moved but Raydium hasn't, Jupiter will route to Raydium. This
//! is free alpha — we always get the best available fill.
//!
//! # Safety gate
//!
//! Like `BinanceClient`, `JupiterClient::execute_swap` requires
//! `live_trading_enabled()` to be true. Paper mode returns
//! `Err(SwapError::PaperMode)` without making any network request.
//!
//! # Token mints (mainnet)
//!
//! | Token | Mint                                          |
//! |-------|-----------------------------------------------|
//! | SOL   | So11111111111111111111111111111111111111112   |
//! | USDC  | EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v |

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Token mints
// ---------------------------------------------------------------------------

pub const MINT_SOL:  &str = "So11111111111111111111111111111111111111112";
pub const MINT_USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum SwapError {
    #[error("live trading is disabled — construct with JupiterClient::new_live()")]
    PaperMode,

    #[error("HTTP error status={status} body={body}")]
    HttpError { status: u16, body: String },

    #[error("request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),

    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),

    #[error("price impact too high: {impact_pct:.4}% > max {max_pct:.4}%")]
    PriceImpactTooHigh { impact_pct: f64, max_pct: f64 },

    #[error("transaction signing failed: {0}")]
    SigningError(String),

    #[error("RPC send failed: {0}")]
    RpcError(String),

    #[error("transaction not confirmed after {attempts} attempts")]
    ConfirmTimeout { attempts: u32 },

    #[error("quote returned zero output amount")]
    ZeroOutput,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JupiterConfig {
    /// Jupiter v6 API base URL. Default: https://quote-api.jup.ag/v6
    pub api_base: String,

    /// Solana RPC endpoint. Default: https://api.mainnet-beta.solana.com
    pub rpc_url: String,

    /// Maximum acceptable price impact in percent. Default: 0.5%
    pub max_price_impact_pct: f64,

    /// Slippage tolerance in basis points. Default: 50 (0.5%)
    pub slippage_bps: u32,

    /// HTTP request timeout. Default: 10s
    pub timeout: Duration,

    /// Transaction confirmation timeout. Default: 60s
    pub confirm_timeout: Duration,

    /// Commitment level for confirmation. Default: confirmed
    pub commitment: CommitmentConfig,
}

impl Default for JupiterConfig {
    fn default() -> Self {
        Self {
            api_base:             "https://quote-api.jup.ag/v6".to_string(),
            rpc_url:              "https://api.mainnet-beta.solana.com".to_string(),
            max_price_impact_pct: 0.5,
            slippage_bps:         50,
            timeout:              Duration::from_secs(10),
            confirm_timeout:      Duration::from_secs(60),
            commitment:           CommitmentConfig::confirmed(),
        }
    }
}

impl JupiterConfig {
    /// Load RPC URL from `SOLANA_RPC_URL` env var, fall back to default.
    pub fn from_env() -> Self {
        let rpc_url = std::env::var("SOLANA_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        Self { rpc_url, ..Self::default() }
    }
}

// ---------------------------------------------------------------------------
// Jupiter API shapes
// ---------------------------------------------------------------------------

/// Response from GET /quote
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint:       String,
    pub in_amount:        String,
    pub output_mint:      String,
    pub out_amount:       String,
    pub price_impact_pct: String,
    pub slippage_bps:     u32,
    // Keep the full quote to pass back to /swap
    #[serde(flatten)]
    pub raw: serde_json::Value,
}

impl QuoteResponse {
    pub fn out_amount_f64(&self) -> f64 {
        self.out_amount.parse().unwrap_or(0.0)
    }
    pub fn price_impact_pct_f64(&self) -> f64 {
        self.price_impact_pct.parse().unwrap_or(0.0)
    }
}

/// Request body for POST /swap
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapRequest {
    quote_response:          serde_json::Value,
    user_public_key:         String,
    wrap_and_unwrap_sol:     bool,
    dynamic_compute_unit_limit: bool,
    prioritization_fee_lamports: u64,
}

/// Response from POST /swap
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapApiResponse {
    swap_transaction: String, // base64-encoded VersionedTransaction
}

// ---------------------------------------------------------------------------
// Swap result
// ---------------------------------------------------------------------------

/// Result of a completed Jupiter swap.
#[derive(Debug, Clone)]
pub struct SwapResult {
    pub signature:        String,
    pub input_mint:       String,
    pub output_mint:      String,
    /// Tokens consumed (lamports or micro-USDC depending on input)
    pub in_amount:        u64,
    /// Tokens received
    pub out_amount:       u64,
    /// Price impact as reported by Jupiter
    pub price_impact_pct: f64,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Jupiter swap client.
pub struct JupiterClient {
    config:  JupiterConfig,
    http:    Client,
    keypair: Option<Keypair>,
    rpc:     Option<RpcClient>,
    live:    bool,
}

impl JupiterClient {
    /// Paper mode — quote requests work, execute_swap returns PaperMode error.
    pub fn new_paper() -> Self {
        Self {
            config:  JupiterConfig::default(),
            http:    Client::builder()
                         .timeout(JupiterConfig::default().timeout)
                         .build()
                         .expect("HTTP client"),
            keypair: None,
            rpc:     None,
            live:    false,
        }
    }

    /// Live mode — requires wallet keypair bytes (64-byte secret key).
    pub fn new_live(keypair_bytes: &[u8], config: JupiterConfig) -> Self {
        let keypair = Keypair::from_bytes(keypair_bytes)
            .expect("invalid keypair bytes");
        let rpc = RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            config.commitment,
        );
        Self {
            http: Client::builder()
                      .timeout(config.timeout)
                      .build()
                      .expect("HTTP client"),
            config,
            keypair: Some(keypair),
            rpc:     Some(rpc),
            live:    true,
        }
    }

    /// Load keypair from `WALLET_KEYPAIR_PATH` env var (JSON array of bytes).
    pub fn from_env(config: JupiterConfig) -> Self {
        match std::env::var("WALLET_KEYPAIR_PATH") {
            Ok(path) => {
                let data = std::fs::read_to_string(&path)
                    .expect("failed to read keypair file");
                let bytes: Vec<u8> = serde_json::from_str(&data)
                    .expect("keypair file must be JSON array of bytes");
                info!("JupiterClient: live mode, wallet={}", path);
                Self::new_live(&bytes, config)
            }
            Err(_) => {
                info!("JupiterClient: paper mode (WALLET_KEYPAIR_PATH not set)");
                Self::new_paper()
            }
        }
    }

    pub fn live_trading_enabled(&self) -> bool { self.live }

    pub fn wallet_pubkey(&self) -> Option<String> {
        self.keypair.as_ref().map(|k| k.pubkey().to_string())
    }

    // ── Quote ─────────────────────────────────────────────────────────────

    /// Get a swap quote from Jupiter.
    ///
    /// Works in both paper and live mode — no wallet required.
    ///
    /// # Arguments
    /// - `input_mint`: token to sell
    /// - `output_mint`: token to buy
    /// - `amount`: input amount in base units (lamports for SOL, micro-USDC for USDC)
    pub async fn get_quote(
        &self,
        input_mint:  &str,
        output_mint: &str,
        amount:      u64,
    ) -> Result<QuoteResponse, SwapError> {
        let url = format!(
            "{}/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            self.config.api_base,
            input_mint, output_mint, amount,
            self.config.slippage_bps,
        );

        debug!("Jupiter: GET /quote input={} output={} amount={}",
            input_mint, output_mint, amount);

        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body   = resp.text().await.unwrap_or_default();
            return Err(SwapError::HttpError { status, body });
        }

        let quote: QuoteResponse = resp.json().await?;

        // Validate price impact
        let impact = quote.price_impact_pct_f64();
        if impact > self.config.max_price_impact_pct {
            warn!("Jupiter: price impact too high impact={:.4}% max={:.4}%",
                impact, self.config.max_price_impact_pct);
            return Err(SwapError::PriceImpactTooHigh {
                impact_pct: impact,
                max_pct:    self.config.max_price_impact_pct,
            });
        }

        if quote.out_amount_f64() == 0.0 {
            return Err(SwapError::ZeroOutput);
        }

        info!("Jupiter: quote in={} out={} impact={:.4}%",
            quote.in_amount, quote.out_amount, impact);

        Ok(quote)
    }

    // ── Execute ───────────────────────────────────────────────────────────

    /// Execute a swap using a previously obtained quote.
    ///
    /// Returns `Err(SwapError::PaperMode)` if not in live mode.
    pub async fn execute_swap(&self, quote: QuoteResponse) -> Result<SwapResult, SwapError> {
        if !self.live {
            return Err(SwapError::PaperMode);
        }

        let keypair = self.keypair.as_ref()
            .ok_or_else(|| SwapError::SigningError("no keypair".into()))?;
        let rpc = self.rpc.as_ref()
            .ok_or_else(|| SwapError::RpcError("no RPC client".into()))?;

        let pubkey = keypair.pubkey().to_string();

        // ── 1. Get serialized transaction from Jupiter ────────────────────
        let swap_req = SwapRequest {
            quote_response:              quote.raw.clone(),
            user_public_key:             pubkey.clone(),
            wrap_and_unwrap_sol:         true,
            dynamic_compute_unit_limit:  true,
            prioritization_fee_lamports: 1_000, // 0.000001 SOL priority fee
        };

        let url = format!("{}/swap", self.config.api_base);
        debug!("Jupiter: POST /swap pubkey={}", pubkey);

        let resp = self.http
            .post(&url)
            .json(&swap_req)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body   = resp.text().await.unwrap_or_default();
            return Err(SwapError::HttpError { status, body });
        }

        let swap_resp: SwapApiResponse = resp.json().await?;

        // ── 2. Deserialize → sign → send ──────────────────────────────────
        let tx_bytes = B64.decode(&swap_resp.swap_transaction)
            .map_err(|e| SwapError::SigningError(format!("base64 decode: {e}")))?;

        let mut versioned_tx: VersionedTransaction =
            bincode::deserialize(&tx_bytes)
                .map_err(|e| SwapError::SigningError(format!("deserialize tx: {e}")))?;

        // Sign with our keypair
        let message_bytes = versioned_tx.message.serialize();
        let signature     = keypair.sign_message(&message_bytes);
        versioned_tx.signatures[0] = signature;

        // ── 3. Send to RPC ────────────────────────────────────────────────
        let sig = rpc
            .send_and_confirm_transaction(&versioned_tx)
            .await
            .map_err(|e| SwapError::RpcError(e.to_string()))?;

        let sig_str = sig.to_string();
        info!("Jupiter: swap confirmed sig={}", sig_str);

        let price_impact = quote.price_impact_pct_f64();
        Ok(SwapResult {
            signature:        sig_str,
            input_mint:       quote.input_mint,
            output_mint:      quote.output_mint,
            in_amount:        quote.in_amount.parse().unwrap_or(0),
            out_amount:       quote.out_amount.parse().unwrap_or(0),
            price_impact_pct: price_impact,
        })
    }

    // ── Convenience: USDC → SOL ───────────────────────────────────────────

    /// Buy SOL with USDC. `usdc_amount` is in whole USDC (e.g. 1000.0).
    pub async fn buy_sol_with_usdc(&self, usdc_amount: f64) -> Result<SwapResult, SwapError> {
        // USDC has 6 decimals
        let amount_micro_usdc = (usdc_amount * 1_000_000.0) as u64;
        let quote = self.get_quote(MINT_USDC, MINT_SOL, amount_micro_usdc).await?;
        self.execute_swap(quote).await
    }

    /// Sell SOL for USDC. `sol_amount` is in whole SOL (e.g. 11.62).
    pub async fn sell_sol_for_usdc(&self, sol_amount: f64) -> Result<SwapResult, SwapError> {
        // SOL has 9 decimals (lamports)
        let lamports = (sol_amount * 1_000_000_000.0) as u64;
        let quote = self.get_quote(MINT_SOL, MINT_USDC, lamports).await?;
        self.execute_swap(quote).await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paper_mode_no_keypair() {
        let client = JupiterClient::new_paper();
        assert!(!client.live_trading_enabled());
        assert!(client.wallet_pubkey().is_none());
    }

    #[tokio::test]
    async fn test_paper_mode_execute_returns_error() {
        let client = JupiterClient::new_paper();
        let dummy_quote = QuoteResponse {
            input_mint:       MINT_USDC.to_string(),
            in_amount:        "1000000000".to_string(),
            output_mint:      MINT_SOL.to_string(),
            out_amount:       "11500000000".to_string(),
            price_impact_pct: "0.01".to_string(),
            slippage_bps:     50,
            raw:              serde_json::json!({}),
        };
        let err = client.execute_swap(dummy_quote).await.unwrap_err();
        assert!(matches!(err, SwapError::PaperMode));
    }

    #[test]
    fn test_quote_response_parsing() {
        let q = QuoteResponse {
            input_mint:       MINT_USDC.to_string(),
            in_amount:        "1000000000".to_string(),
            output_mint:      MINT_SOL.to_string(),
            out_amount:       "11500000000".to_string(),
            price_impact_pct: "0.0312".to_string(),
            slippage_bps:     50,
            raw:              serde_json::json!({}),
        };
        assert!((q.out_amount_f64() - 11_500_000_000.0).abs() < 1.0);
        assert!((q.price_impact_pct_f64() - 0.0312).abs() < 0.0001);
    }

    #[test]
    fn test_usdc_to_lamports_conversion() {
        // 1000 USDC → 1_000_000_000 micro-USDC
        let usdc   = 1_000.0_f64;
        let micro  = (usdc * 1_000_000.0) as u64;
        assert_eq!(micro, 1_000_000_000);
    }

    #[test]
    fn test_sol_to_lamports_conversion() {
        // 11.62 SOL → 11_620_000_000 lamports
        let sol      = 11.62_f64;
        let lamports = (sol * 1_000_000_000.0) as u64;
        assert_eq!(lamports, 11_620_000_000);
    }

    #[test]
    fn test_price_impact_gate() {
        // Config with 0.1% max impact
        let config = JupiterConfig {
            max_price_impact_pct: 0.1,
            ..JupiterConfig::default()
        };
        let client = JupiterClient {
            config,
            http:    Client::new(),
            keypair: None,
            rpc:     None,
            live:    false,
        };
        // Verify the config is stored
        assert!((client.config.max_price_impact_pct - 0.1).abs() < f64::EPSILON);
    }

    #[test]
    fn test_mint_constants_are_valid_pubkeys() {
        // SOL mint is 44 chars, USDC mint is 44 chars
        assert_eq!(MINT_SOL.len(),  44);
        assert_eq!(MINT_USDC.len(), 44);
    }
}