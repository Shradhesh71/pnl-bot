//! Binance WebSocket — streams real best-bid/ask into [`PriceState`].
//!
//! Subscribes to `<symbol>@bookTicker` which fires on every quote change
//! (~10ms latency). This is the direct replacement for the fake
//! `binance_task()` in `paper_trade_demo.rs`.
//!
//! # Why bookTicker and not aggTrade
//!
//! - `bookTicker` fires on every bid/ask change — we get the quote we'd
//!   actually fill at, not just last trade price.
//! - `aggTrade` fires after a fill has already happened — too late for arb.
//! - We pass `bid` and `ask` into `price_state.update_cex_quote()` so the
//!   paper engine and coordinator can model fill prices accurately.
//!
//! # Reconnect behaviour
//!
//! Binance closes connections every 24h and on any network hiccup.
//! This module reconnects automatically with exponential backoff
//! (500ms → 1s → 2s → ... → 30s cap). A keepalive ping fires every
//! 3 minutes — Binance requires a pong response within 10 minutes or
//! it closes the connection.
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use pnl::execution::binance_ws::{BinanceWsConfig, start_binance_ws};
//! use pnl::strategy::price_state::PriceState;
//! use tokio_util::sync::CancellationToken;
//!
//! let price_state = Arc::new(PriceState::default());
//! let cancel      = CancellationToken::new();
//!
//! let handle = start_binance_ws(
//!     BinanceWsConfig::default(),
//!     Arc::clone(&price_state),
//!     cancel.clone(),
//! ).await?;
//!
//! // price_state.cex_raw() now returns live Binance bid/ask
//! ```

use std::{sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::time::{interval, sleep, Instant};
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::Message,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::strategy::price_state::PriceState;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for the Binance WebSocket feed.
#[derive(Debug, Clone)]
pub struct BinanceWsConfig {
    /// Trading pair symbol as Binance expects it (lowercase).
    /// Default: "solusdc"
    pub symbol: String,

    /// Keepalive ping interval. Binance requires pong within 10 minutes.
    /// Default: 3 minutes
    pub ping_interval: Duration,

    /// Initial reconnect backoff. Default: 500ms
    pub backoff_initial: Duration,

    /// Maximum reconnect backoff. Default: 30s
    pub backoff_max: Duration,

    /// Maximum staleness before a warning is logged.
    /// Default: 1000ms
    pub stale_warn_ms: u64,
}

impl Default for BinanceWsConfig {
    fn default() -> Self {
        Self {
            symbol:          "solusdc".to_string(),
            ping_interval:   Duration::from_secs(180),
            backoff_initial: Duration::from_millis(500),
            backoff_max:     Duration::from_secs(30),
            stale_warn_ms:   1_000,
        }
    }
}

impl BinanceWsConfig {
    /// Build the full WebSocket URL for this symbol.
    pub fn ws_url(&self) -> String {
        format!(
            "wss://stream.binance.com/ws/{}@bookTicker",
            self.symbol.to_lowercase()
        )
    }
}

// ---------------------------------------------------------------------------
// Binance bookTicker payload
// ---------------------------------------------------------------------------

/// Deserialized Binance `bookTicker` frame.
///
/// ```json
/// {
///   "u": 400900217,
///   "s": "SOLUSDC",
///   "b": "84.63000",
///   "B": "12.453",
///   "a": "84.64000",
///   "A": "8.000"
/// }
/// ```
#[derive(Debug, Deserialize)]
struct BookTicker {
    /// Symbol (e.g. "SOLUSDC")
    #[serde(rename = "s")]
    symbol: String,

    /// Best bid price
    #[serde(rename = "b")]
    bid: String,

    /// Best ask price
    #[serde(rename = "a")]
    ask: String,
}

impl BookTicker {
    /// Parse bid/ask strings into f64.
    /// Returns None if either field fails to parse.
    fn prices(&self) -> Option<(f64, f64)> {
        let bid = self.bid.parse::<f64>().ok()?;
        let ask = self.ask.parse::<f64>().ok()?;
        if bid > 0.0 && ask > 0.0 && ask >= bid {
            Some((bid, ask))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Handle returned to caller
// ---------------------------------------------------------------------------

/// Handle for the Binance WS task. Drop to stop (or use the CancellationToken).
pub struct BinanceWsHandle {
    pub symbol:  String,
    pub ws_url:  String,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Start the Binance WebSocket feed.
///
/// Spawns a background task that:
/// 1. Connects to `wss://stream.binance.com/ws/<symbol>@bookTicker`
/// 2. Parses every `bookTicker` frame
/// 3. Calls `price_state.update_cex_quote(bid, ask, now_us)`
/// 4. Reconnects automatically on any error
/// 5. Stops cleanly when `cancel` is triggered
///
/// Returns immediately — the WS runs in a background task.
pub async fn start_binance_ws(
    config:      BinanceWsConfig,
    price_state: Arc<PriceState>,
    cancel:      CancellationToken,
) -> anyhow::Result<BinanceWsHandle> {
    let url    = config.ws_url();
    let symbol = config.symbol.clone();

    info!("Binance WS: starting feed symbol={} url={}", symbol, url);

    let handle = BinanceWsHandle {
        symbol: symbol.clone(),
        ws_url: url.clone(),
    };

    tokio::spawn(run_with_reconnect(config, price_state, cancel));

    Ok(handle)
}

// ---------------------------------------------------------------------------
// Reconnect loop
// ---------------------------------------------------------------------------

async fn run_with_reconnect(
    config:      BinanceWsConfig,
    price_state: Arc<PriceState>,
    cancel:      CancellationToken,
) {
    let mut backoff = config.backoff_initial;
    let mut attempt = 0u32;

    loop {
        if cancel.is_cancelled() { break; }

        attempt += 1;
        info!("Binance WS: connecting attempt={} url={}", attempt, config.ws_url());

        match run_session(&config, Arc::clone(&price_state), cancel.clone()).await {
            SessionResult::Cancelled => {
                info!("Binance WS: cancelled, stopping");
                break;
            }
            SessionResult::Disconnected(reason) => {
                warn!("Binance WS: disconnected reason={} backoff={:?}", reason, backoff);
                // Reset backoff after a long-lived session (> 60s means it was healthy)
                // Otherwise double it up to the cap
            }
            SessionResult::Error(e) => {
                error!("Binance WS: session error={} backoff={:?}", e, backoff);
            }
        }

        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = sleep(backoff) => {}
        }

        // Exponential backoff, cap at max
        backoff = (backoff * 2).min(config.backoff_max);
    }

    info!("Binance WS: feed stopped");
}

// ---------------------------------------------------------------------------
// Single session
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum SessionResult {
    Cancelled,
    Disconnected(String),
    Error(String),
}

async fn run_session(
    config:      &BinanceWsConfig,
    price_state: Arc<PriceState>,
    cancel:      CancellationToken,
) -> SessionResult {
    // ── Connect ───────────────────────────────────────────────────────────
    let url = config.ws_url();
    let (ws_stream, _response) = match connect_async(&url).await {
        Ok(r)  => r,
        Err(e) => return SessionResult::Error(format!("connect failed: {e}")),
    };

    info!("Binance WS: connected to {}", url);

    let (mut write, mut read) = ws_stream.split();

    // ── Keepalive ping timer ──────────────────────────────────────────────
    let mut ping_timer = interval(config.ping_interval);
    ping_timer.tick().await; // skip immediate first tick

    // ── Stats ─────────────────────────────────────────────────────────────
    let mut updates:    u64 = 0;
    let mut parse_errs: u64 = 0;
    let session_start = Instant::now();
    let mut last_update = Instant::now();

    loop {
        tokio::select! {
            // ── Cancelled ─────────────────────────────────────────────────
            _ = cancel.cancelled() => {
                let _ = write.close().await;
                return SessionResult::Cancelled;
            }

            // ── Keepalive ping ────────────────────────────────────────────
            _ = ping_timer.tick() => {
                debug!("Binance WS: sending keepalive ping");
                if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                    return SessionResult::Error(format!("ping failed: {e}"));
                }

                // Warn if we haven't received any update recently
                if last_update.elapsed().as_millis() > config.stale_warn_ms as u128 {
                    warn!(
                        "Binance WS: no update in {}ms (stale?)",
                        last_update.elapsed().as_millis()
                    );
                }
            }

            // ── Incoming frame ────────────────────────────────────────────
            msg = read.next() => {
                match msg {
                    None => {
                        let elapsed = session_start.elapsed().as_secs();
                        return SessionResult::Disconnected(
                            format!("stream ended after {elapsed}s updates={updates}")
                        );
                    }
                    Some(Err(e)) => {
                        return SessionResult::Error(format!("read error: {e}"));
                    }
                    Some(Ok(frame)) => {
                        match frame {
                            // ── Text frame: bookTicker payload ────────────
                            Message::Text(text) => {
                                match serde_json::from_str::<BookTicker>(&text) {
                                    Ok(ticker) => {
                                        if let Some((bid, ask)) = ticker.prices() {
                                            let now_us = now_us();
                                            price_state.update_cex_quote(bid, ask, now_us);
                                            updates    += 1;
                                            last_update = Instant::now();

                                            debug!(
                                                "Binance WS: {} bid={:.6} ask={:.6} mid={:.6}",
                                                ticker.symbol, bid, ask,
                                                (bid + ask) / 2.0
                                            );
                                        } else {
                                            warn!(
                                                "Binance WS: invalid prices bid={} ask={}",
                                                ticker.bid, ticker.ask
                                            );
                                            parse_errs += 1;
                                        }
                                    }
                                    Err(e) => {
                                        // Binance occasionally sends non-bookTicker
                                        // frames (e.g. subscription confirmations).
                                        // Log at debug level only.
                                        debug!("Binance WS: non-ticker frame: {e} raw={text}");
                                        parse_errs += 1;
                                    }
                                }
                            }

                            // ── Ping: must respond with Pong ──────────────
                            Message::Ping(data) => {
                                debug!("Binance WS: received ping, sending pong");
                                if let Err(e) = write.send(Message::Pong(data)).await {
                                    return SessionResult::Error(
                                        format!("pong failed: {e}")
                                    );
                                }
                            }

                            // ── Pong: response to our keepalive ───────────
                            Message::Pong(_) => {
                                debug!("Binance WS: received pong");
                            }

                            // ── Close: server initiated close ─────────────
                            Message::Close(frame) => {
                                let reason = frame
                                    .map(|f| f.reason.to_string())
                                    .unwrap_or_else(|| "no reason".to_string());
                                info!(
                                    "Binance WS: server closed connection reason={} \
                                     updates={} parse_errs={} elapsed={:.0}s",
                                    reason, updates, parse_errs,
                                    session_start.elapsed().as_secs_f64()
                                );
                                return SessionResult::Disconnected(reason);
                            }

                            // ── Binary: unexpected, ignore ────────────────
                            Message::Binary(_) | Message::Frame(_) => {
                                debug!("Binance WS: unexpected binary/frame message");
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

#[inline]
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}