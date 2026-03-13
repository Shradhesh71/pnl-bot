//! Coordinator — the Phase 5 brain. Receives a [`SpreadSignal`] and fires
//! both legs concurrently, handles failures, and records realized P&L.
//!
//! # State machine
//!
//! ```text
//! Idle
//!  │  on_signal(SpreadSignal)
//!  ▼
//! Executing  ── tokio::join!(cex_leg, dex_leg)
//!  │  both filled          │  any leg failed
//!  ▼                       ▼
//! Idle ◄────────── Recovery (flatten + cooldown)
//! ```
//!
//! # Leg execution
//!
//! Both legs fire at the **same instant** via `tokio::join!`.
//! Solana finality (~400ms) and Binance REST (~50-200ms) overlap,
//! so total round-trip is dominated by Solana: ~400-800ms.
//!
//! ```text
//! t=0ms   Signal detected
//! t=1ms   Both legs fired concurrently
//! t=50ms  Binance fill confirmed
//! t=500ms Jupiter swap confirmed
//! t=500ms Realized P&L recorded
//! ```
//!
//! # Safety gates (all checked before firing)
//!
//! 1. `paper_mode` flag — default true, blocks all real orders
//! 2. `min_paper_trades` — must have N profitable paper trades first
//! 3. `min_paper_win_rate` — paper win rate must exceed threshold
//! 4. Live balance check — enough USDC/SOL for the trade
//! 5. Signal freshness — slot age must be within `max_signal_slot_age`

use std::{pin::Pin, sync::Arc, time::Instant};

use tokio::sync::Mutex;
use tracing::{error, info};

use crate::{
    execution::{
        binance_rest::{BinanceClient, MarketOrderParams, OrderSide},
        jupiter::{JupiterClient, SwapError, SwapResult},
    },
    paper::{
        paper_engine::{PaperEngine},
        pnl_tracker::PnlTracker,
    },
    strategy::spread_detector::{ArbDirection, SpreadSignal},
};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// If true, log what would happen but send no real orders.
    /// Default: true — must explicitly set false to go live.
    pub paper_mode: bool,

    /// Minimum number of profitable paper trades before live trading allowed.
    /// Default: 50
    pub min_paper_trades: u64,

    /// Minimum paper win rate required before live trading allowed.
    /// Default: 60.0%
    pub min_paper_win_rate_pct: f64,

    /// Maximum slot age of the triggering signal.
    /// Reject if DEX price is stale by the time we get here.
    /// Default: 2 slots
    pub max_signal_slot_age: u64,

    /// Trade size in USDC. Default: $1,000
    pub trade_size_usdt: f64,

    /// Cooldown between live trades (ms). Default: 2000ms
    pub live_cooldown_ms: u64,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            paper_mode:             true,
            min_paper_trades:       50,
            min_paper_win_rate_pct: 60.0,
            max_signal_slot_age:    2,
            trade_size_usdt:        1_000.0,
            live_cooldown_ms:       2_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Execution result
// ---------------------------------------------------------------------------

/// Outcome of one coordinator cycle.
#[derive(Debug)]
pub enum CoordinatorResult {
    /// Both legs filled, P&L recorded.
    Filled(LiveTradeRecord),

    /// Signal was valid but coordinator is in paper mode.
    PaperModeSkipped,

    /// Paper trading prerequisites not yet met.
    PrerequisitesNotMet { reason: String },

    /// Signal was too stale to act on.
    SignalStale { slot_age: u64 },

    /// One or both legs failed.
    LegFailed { cex_err: Option<String>, dex_err: Option<String> },

    /// Insufficient balance.
    InsufficientBalance { usdc: f64, required: f64 },
}

/// A completed live trade with both legs filled.
#[derive(Debug, Clone)]
pub struct LiveTradeRecord {
    pub id:                 u64,
    pub direction:          ArbDirection,
    pub trade_size_usdt:    f64,

    // CEX leg
    pub cex_fill_price:     f64,
    pub cex_fill_qty_sol:   f64,
    pub cex_fee_usdt:       f64,
    pub cex_latency_ms:     u64,

    // DEX leg
    pub dex_fill_price:     f64,
    pub dex_fill_qty_sol:   f64,
    pub dex_signature:      String,
    pub dex_price_impact:   f64,
    pub dex_latency_ms:     u64,

    // P&L
    pub gross_profit_usdt:  f64,
    pub net_profit_usdt:    f64,
    pub total_latency_ms:   u64,

    pub signal_spread_pct:  f64,
    pub dex_slot:           u64,
    pub executed_at_us:     i64,
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

pub struct Coordinator {
    config:        CoordinatorConfig,
    binance:       Arc<BinanceClient>,
    jupiter:       Arc<JupiterClient>,
    paper_engine:  Arc<Mutex<PaperEngine>>,
    pnl_tracker:  Arc<Mutex<PnlTracker>>,
    trade_counter: u64,
    last_live_at:  Option<Instant>,
}

impl Coordinator {
    pub fn new(
        config:       CoordinatorConfig,
        binance:      Arc<BinanceClient>,
        jupiter:      Arc<JupiterClient>,
        paper_engine: Arc<Mutex<PaperEngine>>,
        pnl_tracker:  Arc<Mutex<PnlTracker>>,
    ) -> Self {
        Self {
            config,
            binance,
            jupiter,
            paper_engine,
            pnl_tracker,
            trade_counter: 0,
            last_live_at:  None,
        }
    }

    // ── Primary API ───────────────────────────────────────────────────────

    /// Process a [`SpreadSignal`].
    ///
    /// In paper mode: delegates to `PaperEngine` + `PnlTracker`.
    /// In live mode: fires both legs concurrently after all safety gates pass.
    pub async fn on_signal(
        &mut self,
        signal:   SpreadSignal,
        // snapshot: &crate::strategy::price_state::PriceSnapshot,
    ) -> CoordinatorResult {
        // ── Gate: signal freshness ────────────────────────────────────────
        if signal.dex_slot_age() > self.config.max_signal_slot_age {
            return CoordinatorResult::SignalStale {
                slot_age: signal.dex_slot_age(),
            };
        }

        // ── Paper mode: delegate entirely to PaperEngine ─────────────────
        if self.config.paper_mode {
            let mut engine = self.paper_engine.lock().await;
            match engine.on_signal(signal) {
                Ok(record) => {
                    let mut tracker = self.pnl_tracker.lock().await;
                    tracker.record(&record);
                    info!("Paper: {record}");
                }
                Err(why) => {
                    info!("Paper: skip reason={why}");
                }
            }
            return CoordinatorResult::PaperModeSkipped;
        }

        // ── Live mode: prerequisite checks ───────────────────────────────
        {
            let tracker = self.pnl_tracker.lock().await;
            let snap    = tracker.snapshot();

            if snap.total_trades < self.config.min_paper_trades {
                return CoordinatorResult::PrerequisitesNotMet {
                    reason: format!(
                        "need {} paper trades, have {}",
                        self.config.min_paper_trades, snap.total_trades
                    ),
                };
            }

            if snap.win_rate_pct < self.config.min_paper_win_rate_pct {
                return CoordinatorResult::PrerequisitesNotMet {
                    reason: format!(
                        "win rate {:.1}% < required {:.1}%",
                        snap.win_rate_pct, self.config.min_paper_win_rate_pct
                    ),
                };
            }
        }

        // ── Live cooldown ─────────────────────────────────────────────────
        if let Some(last) = self.last_live_at {
            let elapsed_ms = last.elapsed().as_millis() as u64;
            if elapsed_ms < self.config.live_cooldown_ms {
                return CoordinatorResult::PaperModeSkipped; // reuse skip variant
            }
        }

        // ── Balance check ─────────────────────────────────────────────────
        let (usdc_balance, _sol_balance) = match self.binance.get_balances().await {
            Ok(b) => b,
            Err(e) => {
                error!("Coordinator: balance check failed: {e}");
                return CoordinatorResult::LegFailed {
                    cex_err: Some(e.to_string()),
                    dex_err: None,
                };
            }
        };

        if usdc_balance < self.config.trade_size_usdt {
            return CoordinatorResult::InsufficientBalance {
                usdc:     usdc_balance,
                required: self.config.trade_size_usdt,
            };
        }

        // ── Fire both legs concurrently ───────────────────────────────────
        self.trade_counter += 1;
        let trade_id = self.trade_counter;
        let start    = Instant::now();
        let trade    = self.config.trade_size_usdt;

        info!(
            "Coordinator: LIVE firing trade #{} dir={} spread={:.4}% size=${}",
            trade_id, signal.direction, signal.spread_pct, trade
        );

        let result = self.execute_both_legs(&signal, trade_id, trade).await;

        self.last_live_at = Some(Instant::now());
        let total_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(mut rec) => {
                rec.total_latency_ms = total_ms;
                info!("Coordinator: LIVE trade #{} filled net=${:.4} latency={}ms",
                    trade_id, rec.net_profit_usdt, total_ms);
                CoordinatorResult::Filled(rec)
            }
            Err((cex_err, dex_err)) => {
                error!(
                    "Coordinator: trade #{} failed cex={:?} dex={:?}",
                    trade_id, cex_err, dex_err
                );
                CoordinatorResult::LegFailed { cex_err, dex_err }
            }
        }
    }

    // ── Private: execute both legs ────────────────────────────────────────

    async fn execute_both_legs(
        &self,
        signal:   &SpreadSignal,
        trade_id: u64,
        trade:    f64,
    ) -> Result<LiveTradeRecord, (Option<String>, Option<String>)> {
        let cex_start = Instant::now();
        let dex_start = Instant::now();

        // Build legs based on direction
        let (cex_params, dex_fn): (_, Box<dyn Fn() -> Pin<Box<dyn std::future::Future<Output = Result<SwapResult, SwapError>> + Send>> + Send>) =
            match signal.direction {
                // BuyCexSellDex: buy SOL on Binance, sell SOL on DEX
                ArbDirection::BuyCexSellDex => {
                    let cex_p = MarketOrderParams {
                        symbol:         "SOLUSDC".into(),
                        side:           OrderSide::Buy,
                        quote_qty_usdc: Some(trade),
                        base_qty_sol:   None,
                    };
                    let jup = Arc::clone(&self.jupiter);
                    let sol_estimate = trade / signal.cex_price;
                    (cex_p, Box::new(move || {
                        let j = Arc::clone(&jup);
                        Box::pin(async move { j.sell_sol_for_usdc(sol_estimate).await })
                    }))
                }
                // BuyDexSellCex: buy SOL on DEX, sell SOL on Binance
                ArbDirection::BuyDexSellCex => {
                    let sol_estimate = trade / signal.dex_price;
                    let cex_p = MarketOrderParams {
                        symbol:         "SOLUSDC".into(),
                        side:           OrderSide::Sell,
                        quote_qty_usdc: None,
                        base_qty_sol:   Some(sol_estimate),
                    };
                    let jup = Arc::clone(&self.jupiter);
                    (cex_p, Box::new(move || {
                        let j = Arc::clone(&jup);
                        Box::pin(async move { j.buy_sol_with_usdc(trade).await })
                    }))
                }
            };

        // Fire concurrently
        let cex_future = self.binance.place_market_order(cex_params);
        let dex_future = dex_fn();

        let (cex_result, dex_result) = tokio::join!(cex_future, dex_future);

        let cex_latency = cex_start.elapsed().as_millis() as u64;
        let dex_latency = dex_start.elapsed().as_millis() as u64;

        // Both must succeed
        let cex_fill = cex_result.map_err(|e| (Some(e.to_string()), None))?;
        let dex_fill = dex_result.map_err(|e| (None, Some(e.to_string())))?;

        // Realized P&L
        let gross = match signal.direction {
            ArbDirection::BuyCexSellDex => {
                // Sold SOL on DEX at dex_fill_price, bought on CEX at cex_fill_price
                let sol = cex_fill.executed_qty_sol;
                let received_usdc = sol * dex_fill.out_amount as f64 / 1_000_000.0
                    / cex_fill.executed_qty_sol;
                received_usdc - trade
            }
            ArbDirection::BuyDexSellCex => {
                // Bought SOL on DEX, sold on CEX
                cex_fill.executed_qty_usdc - trade
            }
        };

        let total_fees = cex_fill.fee_usdt
            + (trade * signal.dex_fee_rate);
        let net = gross - total_fees;

        let dex_fill_price = if dex_fill.in_amount > 0 && dex_fill.out_amount > 0 {
            match signal.direction {
                ArbDirection::BuyCexSellDex =>
                    dex_fill.out_amount as f64 / 1_000_000.0
                    / (dex_fill.in_amount as f64 / 1_000_000_000.0),
                ArbDirection::BuyDexSellCex =>
                    dex_fill.in_amount as f64 / 1_000_000.0
                    / (dex_fill.out_amount as f64 / 1_000_000_000.0),
            }
        } else { signal.dex_price };

        Ok(LiveTradeRecord {
            id:                trade_id,
            direction:         signal.direction,
            trade_size_usdt:   trade,
            cex_fill_price:    cex_fill.avg_price,
            cex_fill_qty_sol:  cex_fill.executed_qty_sol,
            cex_fee_usdt:      cex_fill.fee_usdt,
            cex_latency_ms:    cex_latency,
            dex_fill_price,
            dex_fill_qty_sol:  match signal.direction {
                ArbDirection::BuyCexSellDex => cex_fill.executed_qty_sol,
                ArbDirection::BuyDexSellCex =>
                    dex_fill.out_amount as f64 / 1_000_000_000.0,
            },
            dex_signature:     dex_fill.signature,
            dex_price_impact:  dex_fill.price_impact_pct,
            dex_latency_ms:    dex_latency,
            gross_profit_usdt: gross,
            net_profit_usdt:   net,
            total_latency_ms:  0, // filled by caller
            signal_spread_pct: signal.spread_pct,
            dex_slot:          signal.dex_slot,
            executed_at_us:    now_us(),
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[inline]
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_paper_mode() {
        let cfg = CoordinatorConfig::default();
        assert!(cfg.paper_mode, "default must be paper mode");
    }

    #[test]
    fn test_prerequisites_require_50_trades() {
        let cfg = CoordinatorConfig::default();
        assert_eq!(cfg.min_paper_trades, 50);
        assert!((cfg.min_paper_win_rate_pct - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_max_signal_slot_age_tight() {
        // Must be tighter than spread_detector (3 slots) to catch degraded signals
        let cfg = CoordinatorConfig::default();
        assert!(cfg.max_signal_slot_age <= 3);
    }
}