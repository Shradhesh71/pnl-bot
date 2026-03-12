//! P&L tracker — aggregates [`TradeRecord`]s from the paper engine into
//! rolling statistics used by the CLI dashboard.
//!
//! # Design
//!
//! All running totals are updated **incrementally** on each `record()` call —
//! no iteration over trade history. `snapshot()` derives computed metrics
//! (win_rate, sharpe, profit_factor, etc.) from those totals in O(1).
//!
//! # Metrics tracked
//!
//! | Metric           | How computed                                      |
//! |------------------|---------------------------------------------------|
//! | win_rate         | wins / total_trades                               |
//! | avg_win/loss     | running sum / count                               |
//! | profit_factor    | gross_wins / abs(gross_losses)                    |
//! | expectancy       | total_pnl / total_trades                          |
//! | max_drawdown     | peak_balance - trough (incremental)               |
//! | sharpe_raw       | mean(pnl) / std(pnl) — Welford online algorithm   |
//! | trades_per_hour  | total_trades / elapsed_hours                      |
//! | return_pct       | (balance - initial) / initial * 100               |
//!
//! # Usage
//!
//! ```rust,no_run
//! let mut tracker = PnlTracker::new(10_000.0);
//!
//! // After each paper trade:
//! tracker.record(&trade_record);
//!
//! // For CLI dashboard:
//! let snap = tracker.snapshot();
//! println!("{snap}");
//! ```

use std::fmt;
use std::time::{Duration, Instant};

use crate::paper::paper_engine::{TradeOutcome, TradeRecord};

// ---------------------------------------------------------------------------
// PnlTracker
// ---------------------------------------------------------------------------

/// Incremental P&L tracker — updated on each completed trade.
pub struct PnlTracker {
    /// Initial USDT balance (denominator for return_pct)
    initial_balance: f64,

    /// When the tracker was created (for trades_per_hour)
    started_at: Instant,

    // ── Running totals ────────────────────────────────────────────────────

    total_trades: u64,
    wins:         u64,
    losses:       u64,
    breakevens:   u64,

    total_pnl:      f64,  // net P&L across all trades
    gross_pnl:      f64,  // before fees/slippage
    total_fees:     f64,  // all fees paid
    total_slippage: f64,  // all slippage paid

    gross_wins:   f64,    // sum of positive net_pnl
    gross_losses: f64,    // sum of negative net_pnl (stored as negative)

    best_trade:  f64,     // highest single net_pnl
    worst_trade: f64,     // lowest single net_pnl

    // ── Drawdown tracking ─────────────────────────────────────────────────

    peak_balance: f64,    // highest balance seen
    max_drawdown: f64,    // largest peak→trough drop

    // ── Streak tracking ───────────────────────────────────────────────────

    current_wins:           u32,
    current_losses:         u32,
    max_consecutive_wins:   u32,
    max_consecutive_losses: u32,

    // ── Welford online variance (for Sharpe) ──────────────────────────────
    //
    // Welford's algorithm maintains mean and M2 (sum of squared deviations)
    // without storing all values. Numerically stable for thousands of trades.
    //
    //   on each new value x:
    //     count  += 1
    //     delta   = x - mean
    //     mean   += delta / count
    //     delta2  = x - mean        ← uses updated mean
    //     M2     += delta * delta2
    //   variance = M2 / (count - 1) ← sample variance
    //   std      = sqrt(variance)
    welford_mean: f64,
    welford_m2:   f64,

    // ── Last trade snapshot ───────────────────────────────────────────────
    last_trade_id:  u64,
    last_net_pnl:   f64,
    last_balance:   f64,
}

impl PnlTracker {
    /// Create a new tracker with the given initial USDC balance.
    pub fn new(initial_balance: f64) -> Self {
        Self {
            initial_balance,
            started_at:             Instant::now(),
            total_trades:           0,
            wins:                   0,
            losses:                 0,
            breakevens:             0,
            total_pnl:              0.0,
            gross_pnl:              0.0,
            total_fees:             0.0,
            total_slippage:         0.0,
            gross_wins:             0.0,
            gross_losses:           0.0,
            best_trade:             f64::NEG_INFINITY,
            worst_trade:            f64::INFINITY,
            peak_balance:           initial_balance,
            max_drawdown:           0.0,
            current_wins:           0,
            current_losses:         0,
            max_consecutive_wins:   0,
            max_consecutive_losses: 0,
            welford_mean:           0.0,
            welford_m2:             0.0,
            last_trade_id:          0,
            last_net_pnl:           0.0,
            last_balance:           initial_balance,
        }
    }

    // ── Primary API ───────────────────────────────────────────────────────

    /// Record a completed trade. Call this after every successful
    /// `PaperEngine::on_signal`. All updates are O(1).
    pub fn record(&mut self, trade: &TradeRecord) {
        let net = trade.net_profit_usdt;
        let bal = trade.balance_after;

        // ── Counters + streaks ────────────────────────────────────────────
        self.total_trades += 1;

        match trade.outcome {
            TradeOutcome::Win => {
                self.wins        += 1;
                self.gross_wins  += net;
                self.current_wins += 1;
                self.current_losses = 0;
                self.max_consecutive_wins =
                    self.max_consecutive_wins.max(self.current_wins);
            }
            TradeOutcome::Loss => {
                self.losses       += 1;
                self.gross_losses += net; // stays negative
                self.current_losses += 1;
                self.current_wins = 0;
                self.max_consecutive_losses =
                    self.max_consecutive_losses.max(self.current_losses);
            }
            TradeOutcome::Breakeven => {
                self.breakevens    += 1;
                self.current_wins   = 0;
                self.current_losses = 0;
            }
        }

        // ── P&L totals ────────────────────────────────────────────────────
        self.total_pnl      += net;
        self.gross_pnl      += trade.gross_profit_usdt;
        self.total_fees     += trade.cex_fee_usdt + trade.dex_fee_usdt;
        self.total_slippage += trade.slippage_usdt;

        // ── Best / worst ──────────────────────────────────────────────────
        if net > self.best_trade  { self.best_trade  = net; }
        if net < self.worst_trade { self.worst_trade = net; }

        // ── Drawdown (incremental peak-to-trough) ─────────────────────────
        if bal > self.peak_balance { self.peak_balance = bal; }
        let drawdown = self.peak_balance - bal;
        if drawdown > self.max_drawdown { self.max_drawdown = drawdown; }

        // ── Welford online variance ───────────────────────────────────────
        let count  = self.total_trades as f64;
        let delta  = net - self.welford_mean;
        self.welford_mean += delta / count;
        let delta2 = net - self.welford_mean; // re-compute with updated mean
        self.welford_m2   += delta * delta2;

        // ── Last trade ────────────────────────────────────────────────────
        self.last_trade_id = trade.id;
        self.last_net_pnl  = net;
        self.last_balance  = bal;
    }

    // ── Snapshot ──────────────────────────────────────────────────────────

    /// Build a [`PnlSnapshot`] from current state. O(1) — safe to call
    /// at high frequency (e.g. every display refresh).
    pub fn snapshot(&self) -> PnlSnapshot {
        let n = self.total_trades;

        let win_rate_pct = if n > 0 {
            self.wins as f64 / n as f64 * 100.0
        } else { 0.0 };

        let avg_win = if self.wins > 0 {
            self.gross_wins / self.wins as f64
        } else { 0.0 };

        let avg_loss = if self.losses > 0 {
            self.gross_losses / self.losses as f64 // negative
        } else { 0.0 };

        // profit_factor: gross_wins / |gross_losses|
        // Cap at 999.0 when there are no losses yet.
        let profit_factor = if self.gross_losses < -f64::EPSILON {
            (self.gross_wins / self.gross_losses.abs()).min(999.0)
        } else if self.gross_wins > 0.0 {
            999.0
        } else {
            0.0
        };

        let expectancy_usdt = if n > 0 {
            self.total_pnl / n as f64
        } else { 0.0 };

        // Sharpe: mean / std, sample variance needs n > 1
        let sharpe_raw = if n > 1 {
            let variance = self.welford_m2 / (n - 1) as f64;
            let std      = variance.sqrt();
            if std > f64::EPSILON { self.welford_mean / std } else { 0.0 }
        } else { 0.0 };

        let return_pct = (self.last_balance - self.initial_balance)
            / self.initial_balance * 100.0;

        let elapsed         = self.started_at.elapsed();
        let elapsed_hours   = elapsed.as_secs_f64() / 3600.0;
        let trades_per_hour = if elapsed_hours > 0.0 {
            n as f64 / elapsed_hours
        } else { 0.0 };

        PnlSnapshot {
            total_trades:            n,
            wins:                    self.wins,
            losses:                  self.losses,
            breakevens:              self.breakevens,
            win_rate_pct,
            total_pnl_usdt:          self.total_pnl,
            gross_pnl_usdt:          self.gross_pnl,
            total_fees_usdt:         self.total_fees,
            total_slippage_usdt:     self.total_slippage,
            avg_win_usdt:            avg_win,
            avg_loss_usdt:           avg_loss,
            best_trade_usdt:         if n > 0 { self.best_trade  } else { 0.0 },
            worst_trade_usdt:        if n > 0 { self.worst_trade } else { 0.0 },
            profit_factor,
            expectancy_usdt,
            sharpe_raw,
            max_drawdown_usdt:       self.max_drawdown,
            peak_balance_usdt:       self.peak_balance,
            current_balance_usdt:    self.last_balance,
            initial_balance_usdt:    self.initial_balance,
            return_pct,
            trades_per_hour,
            max_consecutive_wins:    self.max_consecutive_wins,
            max_consecutive_losses:  self.max_consecutive_losses,
            current_win_streak:      self.current_wins,
            current_loss_streak:     self.current_losses,
            last_trade_id:           self.last_trade_id,
            last_net_pnl_usdt:       self.last_net_pnl,
            elapsed,
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Total trades recorded so far.
    pub fn total_trades(&self) -> u64 { self.total_trades }

    /// Current running net P&L.
    pub fn total_pnl(&self) -> f64 { self.total_pnl }

    /// True if at least one trade has been recorded.
    pub fn has_trades(&self) -> bool { self.total_trades > 0 }

    /// Reset all state, keeping the same initial balance.
    pub fn reset(&mut self) {
        *self = Self::new(self.initial_balance);
    }
}

// ---------------------------------------------------------------------------
// PnlSnapshot
// ---------------------------------------------------------------------------

/// Point-in-time snapshot of all P&L metrics.
///
/// Returned by [`PnlTracker::snapshot`] — all fields are plain values,
/// safe to clone, log, or send across channels.
#[derive(Debug, Clone)]
pub struct PnlSnapshot {
    // ── Trade counts ──────────────────────────────────────────────────────
    pub total_trades: u64,
    pub wins:         u64,
    pub losses:       u64,
    pub breakevens:   u64,

    // ── Rates ─────────────────────────────────────────────────────────────
    /// Win rate as a percentage (0–100)
    pub win_rate_pct: f64,

    // ── P&L ───────────────────────────────────────────────────────────────
    /// Net P&L across all trades (after fees + slippage)
    pub total_pnl_usdt:      f64,
    /// Gross P&L before fees + slippage
    pub gross_pnl_usdt:      f64,
    /// Total CEX + DEX fees paid
    pub total_fees_usdt:     f64,
    /// Total slippage paid
    pub total_slippage_usdt: f64,

    // ── Per-trade stats ───────────────────────────────────────────────────
    pub avg_win_usdt:     f64,
    pub avg_loss_usdt:    f64,  // negative
    pub best_trade_usdt:  f64,
    pub worst_trade_usdt: f64,

    // ── Risk metrics ──────────────────────────────────────────────────────
    /// gross_wins / |gross_losses|. 999.0 means no losses yet.
    pub profit_factor:     f64,
    /// Average net P&L per trade
    pub expectancy_usdt:   f64,
    /// Per-trade Sharpe ratio (not annualised)
    pub sharpe_raw:        f64,
    /// Largest peak-to-trough balance drop
    pub max_drawdown_usdt: f64,
    /// Highest balance ever reached
    pub peak_balance_usdt: f64,

    // ── Balance ───────────────────────────────────────────────────────────
    pub current_balance_usdt: f64,
    pub initial_balance_usdt: f64,
    /// (current - initial) / initial * 100
    pub return_pct:           f64,

    // ── Throughput ────────────────────────────────────────────────────────
    pub trades_per_hour: f64,

    // ── Streaks ───────────────────────────────────────────────────────────
    pub max_consecutive_wins:   u32,
    pub max_consecutive_losses: u32,
    pub current_win_streak:     u32,
    pub current_loss_streak:    u32,

    // ── Last trade ────────────────────────────────────────────────────────
    pub last_trade_id:     u64,
    pub last_net_pnl_usdt: f64,

    // ── Timing ────────────────────────────────────────────────────────────
    pub elapsed: Duration,
}

impl PnlSnapshot {
    /// True if any trades have been recorded.
    pub fn has_trades(&self) -> bool { self.total_trades > 0 }

    /// Elapsed time as a human-readable string (e.g. "1h 23m 45s").
    pub fn elapsed_human(&self) -> String {
        let secs = self.elapsed.as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0      { format!("{h}h {m:02}m {s:02}s") }
        else if m > 0 { format!("{m}m {s:02}s") }
        else          { format!("{s}s") }
    }

    /// Fee drag: (fees + slippage) / gross_pnl * 100.
    /// Tells you what fraction of gross profit is eaten by costs.
    pub fn fee_drag_pct(&self) -> f64 {
        if self.gross_pnl_usdt.abs() < f64::EPSILON { return 0.0; }
        (self.total_fees_usdt + self.total_slippage_usdt)
            / self.gross_pnl_usdt.abs() * 100.0
    }
}

impl fmt::Display for PnlSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "┌─────────────────────────────────────────────────────┐")?;
        writeln!(f, "│        Paper Trade P&L  ─  {}          │", self.elapsed_human())?;
        writeln!(f, "├─────────────────────────────────────────────────────┤")?;
        writeln!(f, "│  Balance:  ${:.2}   ({:+.4}% return)          │",
            self.current_balance_usdt, self.return_pct)?;
        writeln!(f, "│  Trades:   {}  W:{}  L:{}  BE:{}                    │",
            self.total_trades, self.wins, self.losses, self.breakevens)?;
        writeln!(f, "│  Win Rate: {:.1}%   Profit Factor: {:.2}x            │",
            self.win_rate_pct, self.profit_factor)?;
        writeln!(f, "├─────────────────────────────────────────────────────┤")?;
        writeln!(f, "│  Net PnL:   ${:.4}   Gross: ${:.4}          │",
            self.total_pnl_usdt, self.gross_pnl_usdt)?;
        writeln!(f, "│  Fees:      ${:.4}   Slip:  ${:.4}          │",
            self.total_fees_usdt, self.total_slippage_usdt)?;
        writeln!(f, "│  Avg Win:  ${:.4}   Avg Loss: ${:.4}        │",
            self.avg_win_usdt, self.avg_loss_usdt)?;
        writeln!(f, "│  Best:     ${:.4}   Worst:    ${:.4}        │",
            self.best_trade_usdt, self.worst_trade_usdt)?;
        writeln!(f, "├─────────────────────────────────────────────────────┤")?;
        writeln!(f, "│  Expectancy: ${:.4}/trade   Sharpe: {:.4}    │",
            self.expectancy_usdt, self.sharpe_raw)?;
        writeln!(f, "│  Max DD: ${:.2}   Peak Bal: ${:.2}         │",
            self.max_drawdown_usdt, self.peak_balance_usdt)?;
        writeln!(f, "│  Streak: W{}  L{}  (max W{}  L{})              │",
            self.current_win_streak, self.current_loss_streak,
            self.max_consecutive_wins, self.max_consecutive_losses)?;
        writeln!(f, "│  Rate: {:.1} trades/hr   Fee drag: {:.1}%        │",
            self.trades_per_hour, self.fee_drag_pct())?;
        write!(f,   "└─────────────────────────────────────────────────────┘")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dex::pool_monitor::DexKind,
        paper::paper_engine::{TradeOutcome, TradeRecord},
        strategy::spread_detector::ArbDirection,
    };

    fn make_trade(
        id: u64, net: f64, gross: f64,
        cex_fee: f64, dex_fee: f64, slippage: f64,
        balance_after: f64,
    ) -> TradeRecord {
        TradeRecord {
            id,
            direction:         ArbDirection::BuyCexSellDex,
            dex_source:        DexKind::Orca,
            trade_size_usdt:   1_000.0,
            signal_cex_price:  86.00,
            signal_dex_price:  86.43,
            signal_spread_pct: 0.5,
            fill_cex_price:    86.00,
            fill_dex_price:    86.43,
            cex_fee_usdt:      cex_fee,
            dex_fee_usdt:      dex_fee,
            slippage_usdt:     slippage,
            total_cost_usdt:   cex_fee + dex_fee + slippage,
            gross_profit_usdt: gross,
            net_profit_usdt:   net,
            net_profit_pct:    net / 1_000.0 * 100.0,
            outcome:           TradeOutcome::from_pnl(net),
            balance_after,
            opened_at_us:      0,
            closed_at_us:      0,
            duration_us:       0,
            dex_slot:          405_537_094,
            dex_slot_age:      0,
        }
    }

    /// 10-trade sequence verified in Python:
    /// total_pnl=$21.10, WR=70%, sharpe≈0.9322, maxDD=$1.20, PF≈9.61x
    fn ten_trades() -> Vec<TradeRecord> {
        vec![
            make_trade(1,   3.25,  5.00, 1.00, 0.50,  0.25, 10003.25),
            make_trade(2,   2.10,  3.60, 1.00, 0.50,  0.00, 10005.35),
            make_trade(3,  -0.45,  0.50, 1.00, 0.50, -0.55, 10004.90),
            make_trade(4,   4.80,  6.30, 1.00, 0.50,  0.00, 10009.70),
            make_trade(5,   1.95,  3.45, 1.00, 0.50,  0.00, 10011.65),
            make_trade(6,  -1.20,  0.30, 1.00, 0.50,  0.00, 10010.45),
            make_trade(7,   3.60,  5.10, 1.00, 0.50,  0.00, 10014.05),
            make_trade(8,   2.75,  4.25, 1.00, 0.50,  0.00, 10016.80),
            make_trade(9,  -0.80,  0.70, 1.00, 0.50,  0.00, 10016.00),
            make_trade(10,  5.10,  6.60, 1.00, 0.50,  0.00, 10021.10),
        ]
    }

    #[test]
    fn test_empty_tracker() {
        let t = PnlTracker::new(10_000.0);
        assert!(!t.has_trades());
        assert_eq!(t.total_trades(), 0);
        assert_eq!(t.total_pnl(), 0.0);
        let s = t.snapshot();
        assert_eq!(s.win_rate_pct,  0.0);
        assert_eq!(s.sharpe_raw,    0.0);
        assert_eq!(s.profit_factor, 0.0);
    }

    #[test]
    fn test_trade_counts() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        let s = t.snapshot();
        assert_eq!(s.total_trades, 10);
        assert_eq!(s.wins,  7);
        assert_eq!(s.losses, 3);
        assert_eq!(s.breakevens, 0);
    }

    #[test]
    fn test_win_rate_70_pct() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().win_rate_pct - 70.0).abs() < 0.01);
    }

    #[test]
    fn test_total_pnl_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().total_pnl_usdt - 21.10).abs() < 0.01);
    }

    #[test]
    fn test_avg_win_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().avg_win_usdt - 3.3643).abs() < 0.01);
    }

    #[test]
    fn test_avg_loss_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().avg_loss_usdt - (-0.8167)).abs() < 0.01);
    }

    #[test]
    fn test_best_worst_trade() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        let s = t.snapshot();
        assert!((s.best_trade_usdt  -  5.10).abs() < 0.01);
        assert!((s.worst_trade_usdt - -1.20).abs() < 0.01);
    }

    #[test]
    fn test_profit_factor_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().profit_factor - 9.61).abs() < 0.1);
    }

    #[test]
    fn test_profit_factor_capped_no_losses() {
        let mut t = PnlTracker::new(10_000.0);
        t.record(&make_trade(1, 3.0, 5.0, 1.0, 0.5, 0.5, 10003.0));
        assert_eq!(t.snapshot().profit_factor, 999.0);
    }

    #[test]
    fn test_expectancy_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().expectancy_usdt - 2.11).abs() < 0.01);
    }

    #[test]
    fn test_max_drawdown_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().max_drawdown_usdt - 1.20).abs() < 0.01);
    }

    #[test]
    fn test_drawdown_zero_all_wins() {
        let mut t = PnlTracker::new(10_000.0);
        t.record(&make_trade(1, 3.0, 5.0, 1.0, 0.5, 0.5, 10003.0));
        t.record(&make_trade(2, 2.0, 4.0, 1.0, 0.5, 0.5, 10005.0));
        assert_eq!(t.snapshot().max_drawdown_usdt, 0.0);
    }

    #[test]
    fn test_sharpe_matches_python() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().sharpe_raw - 0.9322).abs() < 0.05);
    }

    #[test]
    fn test_sharpe_zero_one_trade() {
        let mut t = PnlTracker::new(10_000.0);
        t.record(&make_trade(1, 3.0, 5.0, 1.0, 0.5, 0.5, 10003.0));
        assert_eq!(t.snapshot().sharpe_raw, 0.0);
    }

    #[test]
    fn test_sharpe_negative_losses() {
        let mut t = PnlTracker::new(10_000.0);
        t.record(&make_trade(1, -2.0, 0.5, 1.0, 0.5, 1.0, 9998.0));
        t.record(&make_trade(2, -3.0, 0.5, 1.0, 0.5, 2.0, 9995.0));
        assert!(t.snapshot().sharpe_raw < 0.0);
    }

    #[test]
    fn test_welford_stable_large_values() {
        let mut t = PnlTracker::new(1_000_000.0);
        for i in 0..100u64 {
            let net = 1_000.0 + i as f64 * 0.001;
            t.record(&make_trade(i+1, net, net+1.5, 1.0, 0.5, 0.0, 1_000_000.0 + net));
        }
        assert!(t.snapshot().sharpe_raw.is_finite());
    }

    #[test]
    fn test_consecutive_wins() {
        let mut t = PnlTracker::new(10_000.0);
        // W W L W W W L
        for (id, net, bal) in [
            (1, 2.0, 10002.0), (2, 3.0, 10005.0), (3, -1.0, 10004.0),
            (4, 2.0, 10006.0), (5, 1.5, 10007.5), (6, 2.5, 10010.0),
            (7, -0.5, 10009.5),
        ] {
            t.record(&make_trade(id, net, net.abs()+1.5, 1.0, 0.5, 0.0, bal));
        }
        let s = t.snapshot();
        assert_eq!(s.max_consecutive_wins,  3);
        assert_eq!(s.current_loss_streak,   1);
        assert_eq!(s.current_win_streak,    0);
    }

    #[test]
    fn test_consecutive_losses() {
        let mut t = PnlTracker::new(10_000.0);
        for (id, net, bal) in [
            (1, -1.0, 9999.0), (2, -2.0, 9997.0),
            (3, -1.5, 9995.5), (4,  3.0, 9998.5),
        ] {
            t.record(&make_trade(id, net, net.abs()+0.5, 1.0, 0.5, 0.0, bal));
        }
        let s = t.snapshot();
        assert_eq!(s.max_consecutive_losses, 3);
        assert_eq!(s.current_win_streak,     1);
    }

    #[test]
    fn test_return_pct() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        assert!((t.snapshot().return_pct - 0.211).abs() < 0.01);
    }

    #[test]
    fn test_fee_drag_between_0_and_100() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        let d = t.snapshot().fee_drag_pct();
        assert!(d > 0.0 && d < 100.0, "fee_drag={d:.2}");
    }

    #[test]
    fn test_reset() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        t.reset();
        assert!(!t.has_trades());
        assert_eq!(t.total_pnl(), 0.0);
        assert_eq!(t.snapshot().wins, 0);
    }

    #[test]
    fn test_display_contains_key_fields() {
        let mut t = PnlTracker::new(10_000.0);
        for r in ten_trades() { t.record(&r); }
        let s = format!("{}", t.snapshot());
        assert!(s.contains("Balance"),  "missing Balance");
        assert!(s.contains("Win Rate"), "missing Win Rate");
        assert!(s.contains("Sharpe"),   "missing Sharpe");
        assert!(s.contains("Max DD"),   "missing Max DD");
        println!("{s}");
    }
}