//! Spread detector — converts a [`PriceSnapshot`] into a [`SpreadSignal`]
//! when a profitable DEX/CEX price gap is detected.
//!
//! # Role in the pipeline
//!
//! ```text
//! PriceState::snapshot()
//!       ↓
//! SpreadDetector::check(snapshot)
//!       ↓ (only when spread > threshold after fees + slippage)
//! SpreadSignal  ──→  PaperEngine  (Phase 3)
//!               ──→  Coordinator  (Phase 5)
//! ```
//!
//! # Two arbitrage directions
//!
//! ```text
//! BuyCexSellDex:  DEX > CEX  →  buy cheap on Binance, sell expensive on DEX
//!   Example:  Orca=86.43, Binance=86.00, spread=+0.50%
//!   Legs:     (1) Binance market buy SOL/USDC
//!             (2) Orca swap SOL→USDC
//!
//! BuyDexSellCex:  DEX < CEX  →  buy cheap on DEX, sell expensive on Binance
//!   Example:  Orca=85.57, Binance=86.00, spread=-0.50%
//!   Legs:     (1) Orca swap USDC→SOL
//!             (2) Binance market sell SOL/USDC
//! ```
//!
//! # Profit formula
//!
//! ```text
//! gross_profit  = trade_size * spread_abs_pct / 100
//! fee_cost      = trade_size * (dex_fee_pct + cex_taker_fee_pct) / 100
//! slippage_cost = trade_size * slippage_pct / 100
//!                 where slippage_pct = trade_size / (2 * pool_liquidity_usd) * 100
//! net_profit    = gross_profit - fee_cost - slippage_cost
//! ```
//!
//! # Cooldown
//!
//! After emitting a signal the detector blocks for `cooldown_ms` (default 500ms).
//! This prevents firing duplicate signals on consecutive Geyser updates that
//! all reflect the same price move.

use std::{
    fmt,
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
};

use crate::{
    dex::pool_monitor::DexKind,
    strategy::price_state::PriceSnapshot,
};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Full configuration for [`SpreadDetector`].
#[derive(Debug, Clone)]
pub struct SpreadConfig {
    /// Minimum net spread (after fees + slippage) to emit a signal.
    /// Default: 0.15% — covers Orca(0.05%) + Binance(0.10%) + slippage(0.05%)
    pub min_net_spread_pct: f64,

    /// Maximum believable spread — reject anything above this.
    /// No real DEX/CEX arb opportunity ever exceeds 2%.
    /// Default: 2.0%
    pub max_spread_pct: f64,

    /// Notional trade size in USDC for profit calculations.
    /// Default: $1,000 — small enough to avoid large slippage.
    pub trade_size_usdt: f64,

    /// Binance taker fee as a percentage. Default: 0.10%
    pub cex_taker_fee_pct: f64,

    /// Minimum pool liquidity USD required to emit a signal.
    /// Default: $50,000 — ensures slippage estimate is reliable.
    pub min_pool_liquidity_usd: f64,

    /// Cooldown after firing a signal — blocks duplicate signals.
    /// Default: 500ms
    pub cooldown_ms: u64,

    /// Maximum allowed slot age of the DEX price at signal time.
    /// Rejects signals based on stale pool data. Default: 3 slots (~1.2s)
    pub max_signal_slot_age: u64,

    /// Minimum absolute net profit in USDC to emit a signal.
    /// Catches edge cases where pct threshold passes but notional is tiny.
    /// Default: $0.10
    pub min_net_profit_usdt: f64,
}

impl Default for SpreadConfig {
    fn default() -> Self {
        Self {
            min_net_spread_pct:     0.15,
            max_spread_pct:         2.0,
            trade_size_usdt:        1_000.0,
            cex_taker_fee_pct:      0.10,
            min_pool_liquidity_usd: 50_000.0,
            cooldown_ms:            500,
            max_signal_slot_age:    3,
            min_net_profit_usdt:    0.10,
        }
    }
}

impl SpreadConfig {
    /// Total fee percentage for one round trip (DEX + CEX).
    #[inline]
    pub fn total_fee_pct(&self, dex_fee_pct: f64) -> f64 {
        dex_fee_pct + self.cex_taker_fee_pct
    }
}

// ---------------------------------------------------------------------------
// Signal types
// ---------------------------------------------------------------------------

/// Which direction the arbitrage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbDirection {
    /// DEX price > CEX price.
    /// Action: buy cheap on Binance (CEX), sell expensive on DEX.
    /// Leg 1: Binance market buy SOL/USDC
    /// Leg 2: DEX swap SOL → USDC
    BuyCexSellDex,

    /// DEX price < CEX price.
    /// Action: buy cheap on DEX, sell expensive on Binance (CEX).
    /// Leg 1: DEX swap USDC → SOL
    /// Leg 2: Binance market sell SOL/USDC
    BuyDexSellCex,
}

impl fmt::Display for ArbDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuyCexSellDex => write!(f, "BuyCEX→SellDEX"),
            Self::BuyDexSellCex => write!(f, "BuyDEX→SellCEX"),
        }
    }
}

/// A profitable spread opportunity — the output of [`SpreadDetector::check`].
#[derive(Debug, Clone)]
pub struct SpreadSignal {
    /// Trading pair, e.g. "SOLUSDC"
    pub symbol: String,

    /// Which direction to trade
    pub direction: ArbDirection,

    /// CEX (Binance) price at detection time
    pub cex_price: f64,

    /// DEX pool price at detection time
    pub dex_price: f64,

    /// Which DEX this signal is for
    pub dex_source: DexKind,

    /// Signed spread: (dex - cex) / cex * 100
    /// Positive = DEX expensive, Negative = DEX cheap
    pub spread_pct: f64,

    /// Absolute spread percentage
    pub spread_abs_pct: f64,

    /// Gross profit before fees: trade_size * spread_abs_pct / 100
    pub gross_profit_usdt: f64,

    /// Total fee cost: trade_size * (dex_fee + cex_fee) / 100
    pub fee_cost_usdt: f64,

    /// Estimated slippage cost: trade_size * (trade_size / (2 * liquidity)) / 100
    pub slippage_usdt: f64,

    /// Net profit after fees and slippage
    pub net_profit_usdt: f64,

    /// Net profit as a percentage of trade size
    pub net_profit_pct: f64,

    /// Notional trade size used for all calculations
    pub trade_size_usdt: f64,

    /// DEX pool liquidity at time of signal
    pub dex_liquidity_usd: f64,

    /// DEX pool fee rate (e.g. 0.0005 for 0.05%)
    pub dex_fee_rate: f64,

    /// Current Solana slot at detection time
    pub current_slot: u64,

    /// Slot of the DEX price that triggered this signal
    pub dex_slot: u64,

    /// Microsecond timestamp when signal was detected
    pub detected_at_us: i64,
}

impl SpreadSignal {
    /// Slot age of the DEX price that triggered this signal.
    #[inline]
    pub fn dex_slot_age(&self) -> u64 {
        self.current_slot.saturating_sub(self.dex_slot)
    }

    /// True if this is a buy-DEX signal (DEX is cheaper).
    #[inline]
    pub fn is_buy_dex(&self) -> bool {
        self.direction == ArbDirection::BuyDexSellCex
    }

    /// True if this is a sell-DEX signal (DEX is more expensive).
    #[inline]
    pub fn is_sell_dex(&self) -> bool {
        self.direction == ArbDirection::BuyCexSellDex
    }
}

impl fmt::Display for SpreadSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "🎯 SIGNAL [{symbol}] {dir} | \
             spread={spread:+.4}% | \
             CEX={cex:.6} DEX={dex:.6} ({dex_src}) | \
             gross=${gross:.4} fees=${fees:.4} slip=${slip:.4} net=${net:.4} | \
             liq=${liq:.0} slot={slot}",
            symbol  = self.symbol,
            dir     = self.direction,
            spread  = self.spread_pct,
            cex     = self.cex_price,
            dex     = self.dex_price,
            dex_src = self.dex_source,
            gross   = self.gross_profit_usdt,
            fees    = self.fee_cost_usdt,
            slip    = self.slippage_usdt,
            net     = self.net_profit_usdt,
            liq     = self.dex_liquidity_usd,
            slot    = self.dex_slot,
        )
    }
}

// ---------------------------------------------------------------------------
// Detector
// ---------------------------------------------------------------------------

/// Stateful spread detector.
///
/// Call [`SpreadDetector::check`] on every new [`PriceSnapshot`].
/// Returns `Some(SpreadSignal)` only when:
/// 1. Spread exceeds `min_net_spread_pct` after fees and slippage
/// 2. DEX pool has sufficient liquidity
/// 3. DEX price is fresh (within `max_signal_slot_age`)
/// 4. Cooldown since last signal has elapsed
pub struct SpreadDetector {
    config: SpreadConfig,

    /// Microsecond timestamp of the last emitted signal (0 = never).
    last_signal_at_us: AtomicI64,

    /// Total signals emitted since creation.
    signals_emitted: AtomicU64,

    /// Total snapshots checked.
    checks_total: AtomicU64,

    /// Rejections broken down by reason.
    rejected_spread:    AtomicU64,
    rejected_liquidity: AtomicU64,
    rejected_staleness: AtomicU64,
    rejected_cooldown:  AtomicU64,
    rejected_profit:    AtomicU64,
}

impl SpreadDetector {
    /// Create a new detector with the given config.
    pub fn new(config: SpreadConfig) -> Self {
        Self {
            config,
            last_signal_at_us:  AtomicI64::new(0),
            signals_emitted:    AtomicU64::new(0),
            checks_total:       AtomicU64::new(0),
            rejected_spread:    AtomicU64::new(0),
            rejected_liquidity: AtomicU64::new(0),
            rejected_staleness: AtomicU64::new(0),
            rejected_cooldown:  AtomicU64::new(0),
            rejected_profit:    AtomicU64::new(0),
        }
    }

    /// Create with default config.
    pub fn default() -> Self {
        Self::new(SpreadConfig::default())
    }

    /// Check a price snapshot for an arbitrage opportunity.
    ///
    /// Returns `Some(SpreadSignal)` if profitable, `None` otherwise.
    /// This function is designed to be called at high frequency
    /// (hundreds of times per second) with minimal overhead on the
    /// hot path (no allocation unless a signal is emitted).
    pub fn check(&self, snap: &PriceSnapshot) -> Option<SpreadSignal> {
        self.checks_total.fetch_add(1, Ordering::Relaxed);

        let now_us = now_us();

        // ── Gate 1: Cooldown ─────────────────────────────────────────────
        let last = self.last_signal_at_us.load(Ordering::Relaxed);
        if last > 0 {
            let elapsed_ms = ((now_us - last).max(0) / 1_000) as u64;
            if elapsed_ms < self.config.cooldown_ms {
                self.rejected_cooldown.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }

        // ── Gate 2: DEX staleness ────────────────────────────────────────
        let dex_slot_age = snap.current_slot.saturating_sub(snap.best_dex.slot);
        if dex_slot_age > self.config.max_signal_slot_age {
            self.rejected_staleness.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ── Gate 3: Pool liquidity ───────────────────────────────────────
        if snap.best_dex.liquidity_usd < self.config.min_pool_liquidity_usd {
            self.rejected_liquidity.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ── Gate 4: Spread ───────────────────────────────────────────────
        let spread_pct     = (snap.best_dex.price - snap.cex.price) / snap.cex.price * 100.0;
        let spread_abs_pct = spread_pct.abs();
        let dex_fee_pct    = snap.best_dex.fee_rate * 100.0;
        let total_fee_pct  = self.config.total_fee_pct(dex_fee_pct);

        if spread_abs_pct <= total_fee_pct {
            self.rejected_spread.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ── Gate 4.5: sanity cap on spread ───────────────────────────────────
        if spread_abs_pct > self.config.max_spread_pct {
            self.rejected_spread.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ── Gate 5: Net profit after slippage ────────────────────────────
        let trade  = self.config.trade_size_usdt;
        let gross  = trade * spread_abs_pct / 100.0;
        let fees   = trade * total_fee_pct  / 100.0;

        // Slippage estimate: price_impact = trade / (2 * pool_liquidity)
        let price_impact_pct = trade / (2.0 * snap.best_dex.liquidity_usd) * 100.0;
        let slippage         = trade * price_impact_pct / 100.0;

        let net = gross - fees - slippage;

        if net < self.config.min_net_profit_usdt {
            self.rejected_profit.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let net_pct = net / trade * 100.0;

        if net_pct < self.config.min_net_spread_pct {
            self.rejected_spread.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ── All gates passed — emit signal ───────────────────────────────
        self.last_signal_at_us.store(now_us, Ordering::Relaxed);
        self.signals_emitted.fetch_add(1, Ordering::Relaxed);

        let direction = if spread_pct > 0.0 {
            ArbDirection::BuyCexSellDex   // DEX expensive
        } else {
            ArbDirection::BuyDexSellCex   // DEX cheap
        };

        Some(SpreadSignal {
            symbol:             "SOLUSDC".to_string(), // TODO: from snapshot
            direction,
            cex_price:          snap.cex.price,
            dex_price:          snap.best_dex.price,
            dex_source:         snap.best_dex.source,
            spread_pct,
            spread_abs_pct,
            gross_profit_usdt:  gross,
            fee_cost_usdt:      fees,
            slippage_usdt:      slippage,
            net_profit_usdt:    net,
            net_profit_pct:     net_pct,
            trade_size_usdt:    trade,
            dex_liquidity_usd:  snap.best_dex.liquidity_usd,
            dex_fee_rate:       snap.best_dex.fee_rate,
            current_slot:       snap.current_slot,
            dex_slot:           snap.best_dex.slot,
            detected_at_us:     now_us,
        })
    }

    // ── Stats ─────────────────────────────────────────────────────────────

    /// Return a diagnostic stats snapshot.
    pub fn stats(&self) -> DetectorStats {
        DetectorStats {
            checks_total:       self.checks_total.load(Ordering::Relaxed),
            signals_emitted:    self.signals_emitted.load(Ordering::Relaxed),
            rejected_spread:    self.rejected_spread.load(Ordering::Relaxed),
            rejected_liquidity: self.rejected_liquidity.load(Ordering::Relaxed),
            rejected_staleness: self.rejected_staleness.load(Ordering::Relaxed),
            rejected_cooldown:  self.rejected_cooldown.load(Ordering::Relaxed),
            rejected_profit:    self.rejected_profit.load(Ordering::Relaxed),
        }
    }

    /// Reset the cooldown — useful for testing.
    pub fn reset_cooldown(&self) {
        self.last_signal_at_us.store(0, Ordering::Relaxed);
    }

    /// Update trade size at runtime (e.g. based on available balance).
    pub fn config(&self) -> &SpreadConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Counters from the detector — useful for CLI dashboard.
#[derive(Debug, Clone, Copy, Default)]
pub struct DetectorStats {
    pub checks_total:       u64,
    pub signals_emitted:    u64,
    pub rejected_spread:    u64,
    pub rejected_liquidity: u64,
    pub rejected_staleness: u64,
    pub rejected_cooldown:  u64,
    pub rejected_profit:    u64,
}

impl DetectorStats {
    /// Hit rate: signals / checks (as a percentage).
    pub fn hit_rate_pct(&self) -> f64 {
        if self.checks_total == 0 { return 0.0; }
        self.signals_emitted as f64 / self.checks_total as f64 * 100.0
    }
}

impl fmt::Display for DetectorStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "checks={} signals={} hit={:.3}% | \
             rej: spread={} liq={} stale={} cooldown={} profit={}",
            self.checks_total,
            self.signals_emitted,
            self.hit_rate_pct(),
            self.rejected_spread,
            self.rejected_liquidity,
            self.rejected_staleness,
            self.rejected_cooldown,
            self.rejected_profit,
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
        strategy::price_state::{CexPrice, DexEntry, PriceSnapshot},
    };

    // ── Fixtures ─────────────────────────────────────────────────────────

    const SLOT: u64 = 405_537_094;

    fn make_snap(cex: f64, dex: f64, dex_fee: f64, liquidity: f64) -> PriceSnapshot {
        PriceSnapshot {
            cex: CexPrice::from_mid(cex, now_us()),
            best_dex: DexEntry {
                price:          dex,
                fee_rate:       dex_fee,
                liquidity_usd:  liquidity,
                slot:           SLOT,
                received_at_us: now_us(),
                source:         DexKind::Orca,
            },
            orca:    None,
            raydium: None,
            current_slot: SLOT,
            taken_at_us:  now_us(),
        }
    }

    fn detector() -> SpreadDetector {
        SpreadDetector::new(SpreadConfig {
            min_net_spread_pct:     0.15,
            max_spread_pct:         2.0,
            trade_size_usdt:        1_000.0,
            cex_taker_fee_pct:      0.10,
            min_pool_liquidity_usd: 50_000.0,
            cooldown_ms:            0,        // disabled for tests
            max_signal_slot_age:    5,
            min_net_profit_usdt:    0.10,
        })
    }

    // ── Basic signal emission ─────────────────────────────────────────────

    #[test]
    fn test_profitable_signal_emitted() {
        let det = detector();
        // 0.50% spread, Orca fee=0.05%, Binance=0.10% → net ~0.35%
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        let sig = det.check(&snap).expect("should emit signal");
        assert!(sig.net_profit_usdt > 0.0, "net={}", sig.net_profit_usdt);
        assert_eq!(sig.direction, ArbDirection::BuyCexSellDex);
    }

    #[test]
    fn test_buy_dex_direction() {
        let det = detector();
        // DEX cheaper than CEX → buy DEX, sell CEX
        let snap = make_snap(86.00, 85.57, 0.0005, 2_000_000.0);
        let sig = det.check(&snap).expect("should emit signal");
        assert_eq!(sig.direction, ArbDirection::BuyDexSellCex);
        assert!(sig.spread_pct < 0.0, "spread should be negative");
    }

    #[test]
    fn test_no_signal_below_threshold() {
        let det = detector();
        // 0.10% spread — not enough to cover 0.15% fees
        let snap = make_snap(86.00, 86.086, 0.0005, 2_000_000.0);
        assert!(det.check(&snap).is_none());
    }

    #[test]
    fn test_no_signal_at_exact_fee_threshold() {
        let det = detector();
        // Exactly 0.15% spread — net=0 after fees, still blocked
        let snap = make_snap(86.00, 86.129, 0.0005, 2_000_000.0);
        assert!(det.check(&snap).is_none());
    }

    // ── Profit calculation accuracy ───────────────────────────────────────

    #[test]
    fn test_profit_calculation_exact() {
        let det = detector();
        // cex=86.00, dex=86.43
        // spread_abs = (86.43-86.00)/86.00*100 = 0.5000%
        // gross = 1000 * 0.005 = $5.00
        // fees  = 1000 * (0.0005+0.001) = 1000 * 0.0015 = $1.50
        // slippage = 1000 * (1000 / (2*2_000_000)) = 1000 * 0.00025 = $0.25
        // net = 5.00 - 1.50 - 0.25 = $3.25
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        let sig = det.check(&snap).unwrap();

        assert!((sig.gross_profit_usdt - 5.00).abs() < 0.01,
            "gross={:.4}", sig.gross_profit_usdt);
        assert!((sig.fee_cost_usdt - 1.50).abs() < 0.01,
            "fees={:.4}", sig.fee_cost_usdt);
        assert!((sig.slippage_usdt - 0.25).abs() < 0.01,
            "slippage={:.4}", sig.slippage_usdt);
        assert!((sig.net_profit_usdt - 3.25).abs() < 0.05,
            "net={:.4}", sig.net_profit_usdt);
    }

    #[test]
    fn test_raydium_lower_fee_gives_more_profit() {
        let det = detector();
        let snap_orca    = make_snap(86.00, 86.43, 0.0005, 2_000_000.0); // 0.05% fee
        let snap_raydium = make_snap(86.00, 86.43, 0.0001, 2_000_000.0); // 0.01% fee

        let sig_orca    = det.check(&snap_orca).unwrap();
        det.reset_cooldown();
        let sig_raydium = det.check(&snap_raydium).unwrap();

        assert!(
            sig_raydium.net_profit_usdt > sig_orca.net_profit_usdt,
            "Raydium (lower fee) should give more net profit"
        );
    }

    // ── Gates ─────────────────────────────────────────────────────────────

    #[test]
    fn test_gate_low_liquidity() {
        let det = detector();
        // Pool liquidity below min_pool_liquidity_usd ($50k)
        let snap = make_snap(86.00, 86.43, 0.0005, 10_000.0);
        assert!(det.check(&snap).is_none());
        assert_eq!(det.stats().rejected_liquidity, 1);
    }

    #[test]
    fn test_gate_stale_dex() {
        let det = SpreadDetector::new(SpreadConfig {
            max_signal_slot_age: 2,
            cooldown_ms: 0,
            ..SpreadConfig::default()
        });
        let mut snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        // Make DEX price 10 slots old
        snap.best_dex.slot    = SLOT - 10;
        snap.current_slot     = SLOT;
        assert!(det.check(&snap).is_none());
        assert_eq!(det.stats().rejected_staleness, 1);
    }

    #[test]
    fn test_gate_cooldown() {
        let det = SpreadDetector::new(SpreadConfig {
            cooldown_ms: 5_000, // 5 second cooldown
            ..SpreadConfig::default()
        });
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);

        // First signal goes through
        assert!(det.check(&snap).is_some());
        // Second is blocked by cooldown
        assert!(det.check(&snap).is_none());
        assert_eq!(det.stats().rejected_cooldown, 1);
    }

    #[test]
    fn test_gate_min_profit_usdt() {
        let det = SpreadDetector::new(SpreadConfig {
            min_net_profit_usdt: 100.0, // very high minimum
            trade_size_usdt:     1_000.0,
            cooldown_ms:         0,
            ..SpreadConfig::default()
        });
        // $3.25 net profit on $1k trade won't meet $100 minimum
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        assert!(det.check(&snap).is_none());
        assert_eq!(det.stats().rejected_profit, 1);
    }

    // ── Stats ─────────────────────────────────────────────────────────────

    #[test]
    fn test_stats_counts_correctly() {
        let det = detector();
        let good_snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        let bad_snap  = make_snap(86.00, 86.05, 0.0005, 2_000_000.0); // tiny spread

        det.check(&bad_snap);
        det.check(&good_snap);
        det.check(&bad_snap);

        let stats = det.stats();
        assert_eq!(stats.checks_total,    3);
        assert_eq!(stats.signals_emitted, 1);
        assert!(stats.rejected_spread >= 2);
    }

    #[test]
    fn test_hit_rate_zero_with_no_signals() {
        let det = detector();
        let bad = make_snap(86.00, 86.00, 0.0005, 2_000_000.0);
        det.check(&bad);
        assert_eq!(det.stats().hit_rate_pct(), 0.0);
    }

    #[test]
    fn test_hit_rate_nonzero_with_signals() {
        let det = detector();
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        det.check(&snap);
        assert!(det.stats().hit_rate_pct() > 0.0);
    }

    // ── Signal fields ─────────────────────────────────────────────────────

    #[test]
    fn test_signal_fields_populated() {
        let det = detector();
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        let sig = det.check(&snap).unwrap();

        assert_eq!(sig.trade_size_usdt, 1_000.0);
        assert!((sig.dex_fee_rate - 0.0005).abs() < f64::EPSILON);
        assert_eq!(sig.dex_liquidity_usd, 2_000_000.0);
        assert_eq!(sig.dex_slot, SLOT);
        assert_eq!(sig.current_slot, SLOT);
        assert_eq!(sig.dex_slot_age(), 0);
    }

    #[test]
    fn test_is_buy_sell_helpers() {
        let det = detector();

        let snap_sell_dex = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        let sig = det.check(&snap_sell_dex).unwrap();
        assert!(sig.is_sell_dex());
        assert!(!sig.is_buy_dex());

        det.reset_cooldown();

        let snap_buy_dex = make_snap(86.00, 85.57, 0.0005, 2_000_000.0);
        let sig = det.check(&snap_buy_dex).unwrap();
        assert!(sig.is_buy_dex());
        assert!(!sig.is_sell_dex());
    }

    // ── Display ───────────────────────────────────────────────────────────

    #[test]
    fn test_signal_display() {
        let det = detector();
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        let sig = det.check(&snap).unwrap();
        let s = format!("{sig}");
        assert!(s.contains("SIGNAL"),          "missing SIGNAL: {s}");
        assert!(s.contains("BuyCEX→SellDEX"),  "missing direction: {s}");
        assert!(s.contains("86.000000"),        "missing cex price: {s}");
        assert!(s.contains("Orca"),             "missing dex source: {s}");
        println!("{s}");
    }

    #[test]
    fn test_stats_display() {
        let det = detector();
        det.check(&make_snap(86.00, 86.43, 0.0005, 2_000_000.0));
        let s = format!("{}", det.stats());
        assert!(s.contains("checks=1"));
        assert!(s.contains("signals=1"));
        println!("{s}");
    }

    // ── Reset cooldown ────────────────────────────────────────────────────

    #[test]
    fn test_reset_cooldown_allows_second_signal() {
        let det = SpreadDetector::new(SpreadConfig {
            cooldown_ms: 60_000, // 1 minute
            ..SpreadConfig::default()
        });
        let snap = make_snap(86.00, 86.43, 0.0005, 2_000_000.0);
        assert!(det.check(&snap).is_some());
        assert!(det.check(&snap).is_none()); // blocked

        det.reset_cooldown();
        assert!(det.check(&snap).is_some()); // allowed after reset
    }

    // ── Real world numbers from CLI ───────────────────────────────────────

    #[test]
    fn test_real_cli_prices_no_false_signal() {
        // From actual CLI output — no signal should fire here
        // Orca=86.031681, Raydium=85.980797 → intra-DEX spread only ~0.06%
        // With no CEX price yet set, snapshot would be None anyway
        // This test validates our threshold is calibrated correctly
        let det = detector();
        // Simulate Binance price right in the middle
        let snap = make_snap(86.006, 86.031681, 0.0005, 63_005_534.0);
        // spread = (86.031681 - 86.006) / 86.006 * 100 = 0.030%
        // Below 0.15% fee threshold → no signal
        assert!(det.check(&snap).is_none(),
            "Real CLI prices should not trigger a signal");
    }

    #[test]
    fn test_volatile_move_triggers_signal() {
        // Simulate Binance moving +0.5% while DEX lags
        let det = detector();
        let binance_new = 86.43; // moved up 0.5%
        let dex_lagging = 86.00; // DEX hasn't updated yet
        let snap = make_snap(binance_new, dex_lagging, 0.0005, 63_005_534.0);
        // DEX cheaper → buy DEX, sell CEX
        let sig = det.check(&snap).expect("volatile move should trigger signal");
        assert_eq!(sig.direction, ArbDirection::BuyDexSellCex);
        assert!(sig.net_profit_usdt > 0.0);
        println!("Volatile move signal: {sig}");
    }
}