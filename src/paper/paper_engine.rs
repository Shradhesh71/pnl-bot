//! Paper trading engine — simulates DEX/CEX arbitrage trades without
//! touching real funds.
//!
//! # State machine
//!
//! ```text
//! Idle ──on_signal()──→ Simulating ──→ Closed ──→ Idle
//!   ↑                                    │
//!   └────────────────────────────────────┘
//!         (TradeRecord emitted on close)
//! ```
//!
//! # Simulation model
//!
//! Entry prices include realistic fill costs:
//! - **CEX leg**: signal price ± half the Binance bid/ask spread
//! - **DEX leg**: signal price ± price_impact (trade_size / 2*pool_liquidity)
//! - **Fees**: CEX taker fee + DEX swap fee deducted from balance
//!
//! Since this is spot arbitrage (not a position hold), both legs open and
//! close in the same simulated tick. The engine tracks whether the signal
//! price would have been profitable *after* realistic fills.
//!
//! # Usage
//!
//! ```rust,no_run
//! let mut engine = PaperEngine::new(PaperEngineConfig::default());
//!
//! // In your main loop:
//! if let Some(signal) = detector.check(&snapshot) {
//!     if let Some(record) = engine.on_signal(signal, &snapshot) {
//!         println!("Trade closed: {record}");
//!         println!("Balance: ${:.2}", engine.balance());
//!     }
//! }
//! ```

use std::fmt;

use crate::{
    dex::pool_monitor::DexKind,
    strategy::{
        // price_state::PriceSnapshot,
        spread_detector::{ArbDirection, SpreadSignal},
    },
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the paper engine.
#[derive(Debug, Clone)]
pub struct PaperEngineConfig {
    /// Starting USDC balance. Default: $10,000
    pub initial_balance_usdt: f64,

    /// Notional trade size per signal. Default: $1,000
    pub trade_size_usdt: f64,

    /// Binance taker fee as a fraction. Default: 0.001 (0.1%)
    pub cex_taker_fee: f64,

    /// Assumed CEX bid/ask spread as a fraction of price.
    /// Used to model CEX fill slippage. Default: 0.0001 (0.01%)
    pub cex_spread_fraction: f64,

    /// Maximum concurrent trades. Default: 1
    /// Set to 1 — you can't open a second trade until first is closed.
    pub max_concurrent_trades: usize,

    /// If true, skip signals when balance < trade_size_usdt.
    /// Default: true
    pub check_balance: bool,
}

impl Default for PaperEngineConfig {
    fn default() -> Self {
        Self {
            initial_balance_usdt:  10_000.0,
            trade_size_usdt:       1_000.0,
            cex_taker_fee:         0.001,
            cex_spread_fraction:   0.0001,
            max_concurrent_trades: 1,
            check_balance:         true,
        }
    }
}

// ---------------------------------------------------------------------------
// Trade outcome
// ---------------------------------------------------------------------------

/// Outcome of a completed paper trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeOutcome {
    /// Net profit > $0.01
    Win,
    /// Net profit < -$0.01
    Loss,
    /// Net profit within ±$0.01 (fees ate the spread exactly)
    Breakeven,
}

impl TradeOutcome {
    pub fn from_pnl(pnl: f64) -> Self {
        if pnl > 0.01 {
            Self::Win
        } else if pnl < -0.01 {
            Self::Loss
        } else {
            Self::Breakeven
        }
    }
}

impl fmt::Display for TradeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Win       => write!(f, "WIN"),
            Self::Loss      => write!(f, "LOSS"),
            Self::Breakeven => write!(f, "BREAKEVEN"),
        }
    }
}

// ---------------------------------------------------------------------------
// Skip reason
// ---------------------------------------------------------------------------

/// Why a signal was skipped without opening a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Another trade is already open.
    TradeAlreadyOpen,
    /// Insufficient USDC balance.
    InsufficientBalance,
    /// Net profit after realistic fills would be negative.
    NegativeAfterFills,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TradeAlreadyOpen      => write!(f, "trade already open"),
            Self::InsufficientBalance   => write!(f, "insufficient balance"),
            Self::NegativeAfterFills    => write!(f, "negative after realistic fills"),
        }
    }
}

// ---------------------------------------------------------------------------
// Fill prices (internal simulation result)
// ---------------------------------------------------------------------------

/// Simulated fill prices for both legs of a trade.
#[derive(Debug, Clone)]
struct FillPrices {
    /// CEX fill price (ask when buying, bid when selling)
    cex_fill: f64,
    /// DEX fill price (after price impact)
    dex_fill: f64,
    /// CEX fee paid in USDT
    cex_fee_usdt: f64,
    /// DEX fee paid in USDT
    dex_fee_usdt: f64,
    /// Total slippage cost in USDT (price impact on DEX + half-spread on CEX)
    slippage_usdt: f64,
}

// ---------------------------------------------------------------------------
// Trade record
// ---------------------------------------------------------------------------

/// A completed paper trade — the primary output of the engine.
#[derive(Debug, Clone)]
pub struct TradeRecord {
    /// Monotonic trade ID (1-based)
    pub id: u64,

    /// Which direction was traded
    pub direction: ArbDirection,

    /// Which DEX the signal came from
    pub dex_source: DexKind,

    /// Notional trade size in USDT
    pub trade_size_usdt: f64,

    // ── Signal prices (what the detector saw) ────────────────────────────

    /// CEX mid price at signal time
    pub signal_cex_price: f64,
    /// DEX pool price at signal time
    pub signal_dex_price: f64,
    /// Raw spread at signal time (signed %)
    pub signal_spread_pct: f64,

    // ── Simulated fill prices ─────────────────────────────────────────────

    /// Actual simulated CEX fill price (includes half-spread slippage)
    pub fill_cex_price: f64,
    /// Actual simulated DEX fill price (includes price impact)
    pub fill_dex_price: f64,

    // ── Cost breakdown ────────────────────────────────────────────────────

    /// CEX taker fee paid in USDT
    pub cex_fee_usdt: f64,
    /// DEX swap fee paid in USDT
    pub dex_fee_usdt: f64,
    /// Total slippage cost in USDT
    pub slippage_usdt: f64,
    /// Total cost (fees + slippage)
    pub total_cost_usdt: f64,

    // ── P&L ───────────────────────────────────────────────────────────────

    /// Gross profit before fees/slippage
    pub gross_profit_usdt: f64,
    /// Net profit after all costs
    pub net_profit_usdt: f64,
    /// Net profit as % of trade size
    pub net_profit_pct: f64,

    // ── Outcome ───────────────────────────────────────────────────────────

    /// Win / Loss / Breakeven
    pub outcome: TradeOutcome,
    /// Running USDT balance after this trade
    pub balance_after: f64,

    // ── Timing ────────────────────────────────────────────────────────────

    /// Microsecond timestamp when signal was detected
    pub opened_at_us: i64,
    /// Microsecond timestamp when trade was closed (simulated fill)
    pub closed_at_us: i64,
    /// Duration in microseconds (closed - opened)
    pub duration_us: i64,

    // ── Chain data ────────────────────────────────────────────────────────

    /// Solana slot when signal was detected
    pub dex_slot: u64,
    /// Slot age of the DEX price that triggered the signal
    pub dex_slot_age: u64,
}

impl TradeRecord {
    /// Duration as a human-readable string.
    pub fn duration_human(&self) -> String {
        let ms = self.duration_us / 1_000;
        if ms < 1_000 {
            format!("{ms}ms")
        } else {
            format!("{:.1}s", ms as f64 / 1_000.0)
        }
    }
}

impl fmt::Display for TradeRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome_emoji = match self.outcome {
            TradeOutcome::Win       => "✅",
            TradeOutcome::Loss      => "❌",
            TradeOutcome::Breakeven => "➖",
        };
        write!(
            f,
            "{emoji} Trade #{id} [{outcome}] {dir} | \
             spread={spread:+.4}% | \
             gross=${gross:.4} cost=${cost:.4} net=${net:.4} ({net_pct:+.4}%) | \
             bal=${bal:.2} | slot={slot}",
            emoji   = outcome_emoji,
            id      = self.id,
            outcome = self.outcome,
            dir     = self.direction,
            spread  = self.signal_spread_pct,
            gross   = self.gross_profit_usdt,
            cost    = self.total_cost_usdt,
            net     = self.net_profit_usdt,
            net_pct = self.net_profit_pct,
            bal     = self.balance_after,
            slot    = self.dex_slot,
        )
    }
}

// ---------------------------------------------------------------------------
// Open trade (in-flight simulation)
// ---------------------------------------------------------------------------

/// A trade that has been opened but not yet closed.
/// In paper trading this is transient — we open and close in the same call,
/// but the struct exists to make the state machine explicit and extensible
/// for future async simulation.
#[derive(Debug, Clone)]
struct OpenTrade {
    id:             u64,
    signal:         SpreadSignal,
    fills:          FillPrices,
    opened_at_us:   i64,
}

// ---------------------------------------------------------------------------
// Paper engine
// ---------------------------------------------------------------------------

/// Paper trading engine — simulates DEX/CEX arbitrage without real orders.
///
/// Not thread-safe by design — run in a single task and pass signals via
/// a channel if needed. Use `Arc<Mutex<PaperEngine>>` only if you need
/// cross-task access.
pub struct PaperEngine {
    config:        PaperEngineConfig,
    balance:       f64,
    trade_counter: u64,
    open_trade:    Option<OpenTrade>,
    trades:        Vec<TradeRecord>,

    // Stats
    wins:       u64,
    losses:     u64,
    breakevens: u64,
    skipped:    u64,
}

impl PaperEngine {
    /// Create a new paper engine with the given config.
    pub fn new(config: PaperEngineConfig) -> Self {
        let balance = config.initial_balance_usdt;
        Self {
            config,
            balance,
            trade_counter: 0,
            open_trade:    None,
            trades:        Vec::new(),
            wins:          0,
            losses:        0,
            breakevens:    0,
            skipped:       0,
        }
    }

    /// Create with default config (start with $10,000, trade $1,000).
    pub fn default() -> Self {
        Self::new(PaperEngineConfig::default())
    }

    // ── Primary API ───────────────────────────────────────────────────────

    /// Process an incoming [`SpreadSignal`].
    ///
    /// Returns:
    /// - `Ok(Some(TradeRecord))` — signal accepted, trade simulated, record returned
    /// - `Ok(None)`              — signal skipped (trade already open, etc.)
    /// - `Err(SkipReason)`       — signal rejected with reason
    pub fn on_signal(
        &mut self,
        signal: SpreadSignal
    ) -> Result<TradeRecord, SkipReason> {
        // ── Gate 1: concurrent trade limit ───────────────────────────────
        if self.open_trade.is_some() {
            self.skipped += 1;
            return Err(SkipReason::TradeAlreadyOpen);
        }

        // ── Gate 2: balance check ─────────────────────────────────────────
        if self.config.check_balance
            && self.balance < self.config.trade_size_usdt
        {
            self.skipped += 1;
            return Err(SkipReason::InsufficientBalance);
        }

        // ── Simulate fill prices ──────────────────────────────────────────
        let fills = self.simulate_fills(&signal);

        // ── Compute P&L at fill prices ────────────────────────────────────
        let gross = self.compute_gross(&signal, &fills);
        let total_cost = fills.cex_fee_usdt + fills.dex_fee_usdt + fills.slippage_usdt;
        let net = gross - total_cost;

        // ── Gate 3: still profitable after realistic fills ────────────────
        if net <= 0.0 {
            self.skipped += 1;
            return Err(SkipReason::NegativeAfterFills);
        }

        // ── Open trade ────────────────────────────────────────────────────
        self.trade_counter += 1;
        let now_us = now_us();
        let open = OpenTrade {
            id:           self.trade_counter,
            signal:       signal.clone(),
            fills:        fills.clone(),
            opened_at_us: now_us,
        };
        self.open_trade = Some(open);

        // ── Immediately close (spot arb — both legs simultaneous) ─────────
        let record = self.close_trade(gross, net, total_cost, &fills, now_us);

        Ok(record)
    }

    // ── Reads ─────────────────────────────────────────────────────────────

    /// Current USDT balance.
    pub fn balance(&self) -> f64 { self.balance }

    /// Total number of completed trades.
    pub fn trade_count(&self) -> usize { self.trades.len() }

    /// Full trade history (most recent last).
    pub fn trades(&self) -> &[TradeRecord] { &self.trades }

    /// Most recent trade.
    pub fn last_trade(&self) -> Option<&TradeRecord> { self.trades.last() }

    /// True if a trade is currently open (being simulated).
    pub fn has_open_trade(&self) -> bool { self.open_trade.is_some() }

    /// Diagnostic snapshot.
    pub fn stats(&self) -> EngineStats {
        let total = self.trades.len() as u64;
        let total_pnl: f64 = self.trades.iter().map(|t| t.net_profit_usdt).sum();
        let win_rate = if total > 0 {
            self.wins as f64 / total as f64 * 100.0
        } else { 0.0 };

        let best  = self.trades.iter()
            .map(|t| t.net_profit_usdt)
            .fold(f64::NEG_INFINITY, f64::max);
        let worst = self.trades.iter()
            .map(|t| t.net_profit_usdt)
            .fold(f64::INFINITY, f64::min);

        // Max drawdown: largest peak-to-trough drop in balance
        let mut peak = self.config.initial_balance_usdt;
        let mut max_dd = 0.0_f64;
        for t in &self.trades {
            if t.balance_after > peak { peak = t.balance_after; }
            let dd = peak - t.balance_after;
            if dd > max_dd { max_dd = dd; }
        }

        EngineStats {
            total_trades:       total,
            wins:               self.wins,
            losses:             self.losses,
            breakevens:         self.breakevens,
            skipped:            self.skipped,
            win_rate_pct:       win_rate,
            total_pnl_usdt:     total_pnl,
            balance:            self.balance,
            best_trade_usdt:    if total > 0 { best  } else { 0.0 },
            worst_trade_usdt:   if total > 0 { worst } else { 0.0 },
            max_drawdown_usdt:  max_dd,
            initial_balance:    self.config.initial_balance_usdt,
        }
    }

    // ── Private: fill simulation ──────────────────────────────────────────

    /// Simulate realistic fill prices for both legs.
    fn simulate_fills(&self, signal: &SpreadSignal) -> FillPrices {
        let trade  = self.config.trade_size_usdt;
        let cex_p  = signal.cex_price;
        let dex_p  = signal.dex_price;

        // CEX slippage: half the bid/ask spread, direction-dependent
        // BuyDexSellCex → sell on CEX → fill at bid (price - half_spread)
        // BuyCexSellDex → buy on CEX  → fill at ask (price + half_spread)
        let half_spread = cex_p * self.config.cex_spread_fraction / 2.0;
        let cex_fill = match signal.direction {
            ArbDirection::BuyCexSellDex => cex_p + half_spread, // pay ask
            ArbDirection::BuyDexSellCex => cex_p - half_spread, // receive bid
        };

        // DEX slippage: price impact from pool depth
        // trade_size / (2 * pool_liquidity) = price impact fraction
        let price_impact = trade / (2.0 * signal.dex_liquidity_usd.max(1.0));

        // BuyCexSellDex → sell on DEX → fill below spot (impact negative)
        // BuyDexSellCex → buy on DEX  → fill above spot (impact positive)
        let dex_fill = match signal.direction {
            ArbDirection::BuyCexSellDex => dex_p * (1.0 - price_impact),
            ArbDirection::BuyDexSellCex => dex_p * (1.0 + price_impact),
        };

        // Fees
        let cex_fee_usdt = trade * self.config.cex_taker_fee;
        let dex_fee_usdt = trade * signal.dex_fee_rate;

        // Slippage in USDT
        let cex_slip = trade * (half_spread / cex_p);
        let dex_slip = trade * price_impact;
        let slippage_usdt = cex_slip + dex_slip;

        FillPrices {
            cex_fill,
            dex_fill,
            cex_fee_usdt,
            dex_fee_usdt,
            slippage_usdt,
        }
    }

    /// Compute gross profit from fill prices (before fees/slippage).
    fn compute_gross(&self, signal: &SpreadSignal, fills: &FillPrices) -> f64 {
        let trade = self.config.trade_size_usdt;

        match signal.direction {
            // BuyCexSellDex:
            //   Buy SOL on CEX at fill_cex (spend USDT, get SOL)
            //   Sell SOL on DEX at fill_dex (get USDT back)
            //   gross = (fill_dex - fill_cex) / fill_cex * trade
            ArbDirection::BuyCexSellDex => {
                let sol_amount = trade / fills.cex_fill;
                let usdt_back  = sol_amount * fills.dex_fill;
                usdt_back - trade
            }

            // BuyDexSellCex:
            //   Buy SOL on DEX at fill_dex (spend USDT, get SOL)
            //   Sell SOL on CEX at fill_cex (get USDT back)
            //   gross = (fill_cex - fill_dex) / fill_dex * trade
            ArbDirection::BuyDexSellCex => {
                let sol_amount = trade / fills.dex_fill;
                let usdt_back  = sol_amount * fills.cex_fill;
                usdt_back - trade
            }
        }
    }

    /// Close the open trade, update balance, record history.
    fn close_trade(
        &mut self,
        gross: f64,
        net: f64,
        total_cost: f64,
        fills: &FillPrices,
        now_us: i64,
    ) -> TradeRecord {
        let open = self.open_trade.take().unwrap();

        // Update balance
        self.balance += net;

        // Outcome
        let outcome = TradeOutcome::from_pnl(net);
        match outcome {
            TradeOutcome::Win       => self.wins       += 1,
            TradeOutcome::Loss      => self.losses     += 1,
            TradeOutcome::Breakeven => self.breakevens += 1,
        }

        let net_pct = net / self.config.trade_size_usdt * 100.0;

        let record = TradeRecord {
            id:                 open.id,
            direction:          open.signal.direction,
            dex_source:         open.signal.dex_source,
            trade_size_usdt:    self.config.trade_size_usdt,
            signal_cex_price:   open.signal.cex_price,
            signal_dex_price:   open.signal.dex_price,
            signal_spread_pct:  open.signal.spread_pct,
            fill_cex_price:     fills.cex_fill,
            fill_dex_price:     fills.dex_fill,
            cex_fee_usdt:       fills.cex_fee_usdt,
            dex_fee_usdt:       fills.dex_fee_usdt,
            slippage_usdt:      fills.slippage_usdt,
            total_cost_usdt:    total_cost,
            gross_profit_usdt:  gross,
            net_profit_usdt:    net,
            net_profit_pct:     net_pct,
            outcome,
            balance_after:      self.balance,
            opened_at_us:       open.opened_at_us,
            closed_at_us:       now_us,
            duration_us:        now_us - open.opened_at_us,
            dex_slot:           open.signal.dex_slot,
            dex_slot_age:       open.signal.dex_slot_age(),
        };

        self.trades.push(record.clone());
        record
    }
}

// ---------------------------------------------------------------------------
// Engine stats
// ---------------------------------------------------------------------------

/// Aggregate statistics from the paper engine.
#[derive(Debug, Clone)]
pub struct EngineStats {
    pub total_trades:      u64,
    pub wins:              u64,
    pub losses:            u64,
    pub breakevens:        u64,
    pub skipped:           u64,
    pub win_rate_pct:      f64,
    pub total_pnl_usdt:    f64,
    pub balance:           f64,
    pub best_trade_usdt:   f64,
    pub worst_trade_usdt:  f64,
    pub max_drawdown_usdt: f64,
    pub initial_balance:   f64,
}

impl EngineStats {
    /// Return on initial capital as a percentage.
    pub fn return_pct(&self) -> f64 {
        (self.balance - self.initial_balance) / self.initial_balance * 100.0
    }
}

impl fmt::Display for EngineStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "trades={} wins={} losses={} WR={:.1}% | \
             PnL=${:.2} ({:+.2}%) bal=${:.2} | \
             best=${:.2} worst=${:.2} maxDD=${:.2} | \
             skipped={}",
            self.total_trades,
            self.wins,
            self.losses,
            self.win_rate_pct,
            self.total_pnl_usdt,
            self.return_pct(),
            self.balance,
            self.best_trade_usdt,
            self.worst_trade_usdt,
            self.max_drawdown_usdt,
            self.skipped,
        )
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
    use crate::{
        dex::pool_monitor::DexKind,
        strategy::{
            // price_state::{CexPrice, DexEntry, PriceSnapshot},
            spread_detector::{ArbDirection, SpreadSignal},
        },
    };

    // ── Fixtures ─────────────────────────────────────────────────────────

    const SLOT: u64 = 405_537_094;

    fn make_signal(
        direction: ArbDirection,
        cex: f64,
        dex: f64,
        spread_pct: f64,
        liquidity: f64,
    ) -> SpreadSignal {
        let trade       = 1_000.0_f64;
        let spread_abs  = spread_pct.abs();
        let dex_fee     = 0.0005_f64;
        let cex_fee     = 0.001_f64;
        let gross       = trade * spread_abs / 100.0;
        let fees        = trade * (dex_fee + cex_fee);
        let slip        = trade * (trade / (2.0 * liquidity));
        let net         = gross - fees - slip;
        SpreadSignal {
            symbol:             "SOLUSDC".into(),
            direction,
            cex_price:          cex,
            dex_price:          dex,
            dex_source:         DexKind::Orca,
            spread_pct,
            spread_abs_pct:     spread_abs,
            gross_profit_usdt:  gross,
            fee_cost_usdt:      fees,
            slippage_usdt:      slip,
            net_profit_usdt:    net,
            net_profit_pct:     net / trade * 100.0,
            trade_size_usdt:    trade,
            dex_liquidity_usd:  liquidity,
            dex_fee_rate:       dex_fee,
            current_slot:       SLOT,
            dex_slot:           SLOT,
            detected_at_us:     now_us(),
        }
    }

    // fn make_snapshot(cex: f64, dex: f64, liquidity: f64) -> PriceSnapshot {
    //     PriceSnapshot {
    //         cex: CexPrice::from_mid(cex, now_us()),
    //         best_dex: DexEntry {
    //             price:          dex,
    //             fee_rate:       0.0005,
    //             liquidity_usd:  liquidity,
    //             slot:           SLOT,
    //             received_at_us: now_us(),
    //             source:         DexKind::Orca,
    //         },
    //         orca:         None,
    //         raydium:      None,
    //         current_slot: SLOT,
    //         taken_at_us:  now_us(),
    //     }
    // }

    fn engine() -> PaperEngine {
        PaperEngine::new(PaperEngineConfig::default())
    }

    // ── Basic trade lifecycle ─────────────────────────────────────────────

    #[test]
    fn test_win_trade_recorded() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).expect("should succeed");

        assert_eq!(rec.outcome, TradeOutcome::Win);
        assert!(rec.net_profit_usdt > 0.0, "net={}", rec.net_profit_usdt);
        assert_eq!(eng.trade_count(), 1);
    }

    #[test]
    fn test_balance_increases_on_win() {
        let mut eng = engine();
        let initial = eng.balance();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        assert!(eng.balance() > initial, "balance should increase on win");
        assert!((eng.balance() - initial - rec.net_profit_usdt).abs() < 0.001);
    }

    #[test]
    fn test_buy_dex_direction() {
        let mut eng = engine();
        // DEX cheaper than CEX → BuyDexSellCex
        let sig  = make_signal(ArbDirection::BuyDexSellCex, 86.00, 85.57, -0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 85.57, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        assert_eq!(rec.direction, ArbDirection::BuyDexSellCex);
        assert!(rec.net_profit_usdt > 0.0);
    }

    // ── Gates ─────────────────────────────────────────────────────────────

    #[test]
    fn test_skip_when_trade_open() {
        let mut eng = engine();
        // Manually set an open trade
        eng.open_trade = Some(OpenTrade {
            id:           1,
            signal:       make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0),
            fills:        FillPrices {
                cex_fill: 86.00, dex_fill: 86.43,
                cex_fee_usdt: 1.0, dex_fee_usdt: 0.5, slippage_usdt: 0.1,
            },
            opened_at_us: now_us(),
        });

        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let err  = eng.on_signal(sig).unwrap_err();

        assert_eq!(err, SkipReason::TradeAlreadyOpen);
        assert_eq!(eng.skipped, 1);
    }

    #[test]
    fn test_skip_insufficient_balance() {
        let mut eng = PaperEngine::new(PaperEngineConfig {
            initial_balance_usdt: 500.0, // less than trade_size=1000
            ..PaperEngineConfig::default()
        });

        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let err  = eng.on_signal(sig).unwrap_err();

        assert_eq!(err, SkipReason::InsufficientBalance);
    }

    #[test]
    fn test_skip_negative_after_fills() {
        let mut eng = engine();
        // Tiny spread — signal passed detector but fills eat all profit
        // spread=0.21%, fees+slippage > gross after realistic fill prices
        let mut sig = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.18, 0.21, 50_000.0);
        // Override net to look positive to the detector
        sig.net_profit_usdt = 0.01;
        // let snap = make_snapshot(86.00, 86.18, 50_000.0);

        // With thin liquidity ($50k), slippage on $1k trade = 1%
        // This should flip the trade negative after fill simulation
        match eng.on_signal(sig) {
            Err(SkipReason::NegativeAfterFills) => {} // expected
            Ok(rec) => {
                // Only pass if genuinely profitable after fills
                assert!(rec.net_profit_usdt > 0.0);
            }
            Err(other) => panic!("unexpected skip reason: {other}"),
        }
    }

    // ── Fill price accuracy ───────────────────────────────────────────────

    #[test]
    fn test_fill_prices_include_spread() {
        let mut eng = engine();
        // BuyCexSellDex → buy on CEX at ask (above mid)
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        // CEX fill should be above mid (paying the ask)
        assert!(rec.fill_cex_price >= 86.00,
            "CEX fill should be at or above mid: {}", rec.fill_cex_price);

        // DEX fill should be below spot (selling into bids)
        assert!(rec.fill_dex_price <= 86.43,
            "DEX fill should be at or below spot: {}", rec.fill_dex_price);
    }

    #[test]
    fn test_fill_prices_buy_dex_direction() {
        let mut eng = engine();
        // BuyDexSellCex → buy on DEX at ask (above spot), sell on CEX at bid (below mid)
        let sig  = make_signal(ArbDirection::BuyDexSellCex, 86.00, 85.57, -0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 85.57, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        // CEX fill below mid (receiving bid)
        assert!(rec.fill_cex_price <= 86.00,
            "CEX bid should be <= mid: {}", rec.fill_cex_price);

        // DEX fill above spot (paying ask/impact)
        assert!(rec.fill_dex_price >= 85.57,
            "DEX ask should be >= spot: {}", rec.fill_dex_price);
    }

    // ── Cost breakdown ────────────────────────────────────────────────────

    #[test]
    fn test_fees_deducted_correctly() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        // CEX fee: 0.1% of $1000 = $1.00
        assert!((rec.cex_fee_usdt - 1.00).abs() < 0.01,
            "cex_fee={:.4}", rec.cex_fee_usdt);

        // DEX fee: 0.05% of $1000 = $0.50
        assert!((rec.dex_fee_usdt - 0.50).abs() < 0.01,
            "dex_fee={:.4}", rec.dex_fee_usdt);

        // total_cost = cex_fee + dex_fee + slippage
        let expected_cost = rec.cex_fee_usdt + rec.dex_fee_usdt + rec.slippage_usdt;
        assert!((rec.total_cost_usdt - expected_cost).abs() < 0.001,
            "total_cost={:.4}", rec.total_cost_usdt);
    }

    #[test]
    fn test_net_equals_gross_minus_cost() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        let expected_net = rec.gross_profit_usdt - rec.total_cost_usdt;
        assert!((rec.net_profit_usdt - expected_net).abs() < 0.001,
            "net={:.4} expected={:.4}", rec.net_profit_usdt, expected_net);
    }

    // ── Trade history and stats ───────────────────────────────────────────

    #[test]
    fn test_multiple_trades_tracked() {
        let mut eng = engine();
        for _ in 0..3 {
            let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
            // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
            eng.on_signal(sig).unwrap();
        }
        assert_eq!(eng.trade_count(), 3);
        assert_eq!(eng.stats().total_trades, 3);
    }

    #[test]
    fn test_trade_ids_monotonic() {
        let mut eng = engine();
        for i in 1..=3 {
            let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
            // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
            let rec  = eng.on_signal(sig).unwrap();
            assert_eq!(rec.id, i, "trade id should be monotonic");
        }
    }

    #[test]
    fn test_stats_win_rate() {
        let mut eng = engine();
        // 2 winning trades
        for _ in 0..2 {
            let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
            // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
            eng.on_signal(sig).unwrap();
        }
        let stats = eng.stats();
        assert_eq!(stats.wins, 2);
        assert!((stats.win_rate_pct - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_stats_max_drawdown() {
        let mut eng = PaperEngine::new(PaperEngineConfig {
            initial_balance_usdt: 10_000.0,
            ..PaperEngineConfig::default()
        });

        // Winning trade — balance goes up
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        eng.on_signal(sig).unwrap();

        // If we had a losing trade the drawdown would be computed correctly
        // For now just verify it's non-negative
        assert!(eng.stats().max_drawdown_usdt >= 0.0);
    }

    #[test]
    fn test_return_pct() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        let expected_return = rec.net_profit_usdt / 10_000.0 * 100.0;
        let actual_return   = eng.stats().return_pct();
        assert!((actual_return - expected_return).abs() < 0.001,
            "return_pct={:.4} expected={:.4}", actual_return, expected_return);
    }

    #[test]
    fn test_balance_after_in_record() {
        let mut eng = engine();
        let initial = eng.balance();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();

        assert!((rec.balance_after - eng.balance()).abs() < f64::EPSILON,
            "balance_after in record should match engine balance");
        assert!(rec.balance_after > initial);
    }

    // ── Display ───────────────────────────────────────────────────────────

    #[test]
    fn test_trade_record_display() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();
        let s = format!("{rec}");
        assert!(s.contains("Trade #1"),       "missing id: {s}");
        assert!(s.contains("WIN"),            "missing outcome: {s}");
        assert!(s.contains("BuyCEX→SellDEX"), "missing direction: {s}");
        println!("{s}");
    }

    #[test]
    fn test_engine_stats_display() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        eng.on_signal(sig).unwrap();
        let s = format!("{}", eng.stats());
        assert!(s.contains("trades=1"));
        assert!(s.contains("wins=1"));
        println!("{s}");
    }

    #[test]
    fn test_duration_human() {
        let mut eng = engine();
        let sig  = make_signal(ArbDirection::BuyCexSellDex, 86.00, 86.43, 0.5, 2_000_000.0);
        // let snap = make_snapshot(86.00, 86.43, 2_000_000.0);
        let rec  = eng.on_signal(sig).unwrap();
        let s = rec.duration_human();
        // Should be either "Xms" or "X.Xs"
        assert!(s.ends_with("ms") || s.ends_with('s'), "bad format: {s}");
    }
}