//! Shared price state — thread-safe bridge between data sources and detector.
//!
//! # Role in the pipeline
//!
//! ```text
//! Binance WS task  ──→  PriceState::update_cex()   ─┐
//!                                                     ├──→  PriceState::snapshot()
//! Geyser task      ──→  PriceState::update_dex()   ─┘          ↓
//!                                                         SpreadDetector
//! ```
//!
//! # Design
//!
//! - **Lock-free writes** via `crossbeam::atomic::AtomicCell` — the CEX and
//!   each DEX source each have their own atomic slot, so writers never block
//!   each other.
//! - **Separate Orca and Raydium slots** — `best_dex()` picks the fresher
//!   of the two. This lets the spread detector use whichever pool had a more
//!   recent update without discarding the other.
//! - **Staleness guards** — CEX price older than `max_cex_age_ms` and DEX
//!   price older than `max_dex_slot_age` slots are treated as absent.
//! - **`Arc<PriceState>`** — clone the Arc to share across tasks; the inner
//!   state is fully `Send + Sync`.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use crate::strategy::price_state::{PriceState, PriceStateConfig};
//! use crate::dex::pool_monitor::PoolPrice;
//!
//! let state = Arc::new(PriceState::new(PriceStateConfig::default()));
//!
//! // In CEX WS task:
//! state.update_cex(148.32, now_us());
//!
//! // In Geyser task:
//! state.update_dex(pool_price);
//! state.update_slot(285_441_234);
//!
//! // In spread detector:
//! if let Some(snap) = state.snapshot() {
//!     let spread = (snap.best_dex.price - snap.cex.price) / snap.cex.price;
//! }
//! ```

use std::{
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
    }
};

use crossbeam::atomic::AtomicCell;

use crate::dex::pool_monitor::{DexKind, PoolPrice};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Staleness thresholds and behaviour config for [`PriceState`].
#[derive(Debug, Clone)]
pub struct PriceStateConfig {
    /// Reject CEX price older than this (default: 500ms).
    /// Binance WS typically sends updates every 100ms; 500ms = 5 missed updates.
    pub max_cex_age_ms: u64,

    /// Reject DEX price older than this many slots (default: 5 slots ≈ 2s).
    /// Solana produces ~2.5 slots/sec; 5 slots ≈ 2 seconds.
    pub max_dex_slot_age: u64,

    /// Minimum DEX liquidity USD to accept (default: $10,000).
    /// Rejects thin pools where slippage would exceed spread.
    pub min_dex_liquidity_usd: f64,

    /// Minimum sane price — reject anything below (default: $0.000001).
    pub min_price: f64,

    /// Maximum sane price — reject anything above (default: $10,000,000).
    pub max_price: f64,
}

impl Default for PriceStateConfig {
    fn default() -> Self {
        Self {
            max_cex_age_ms:        500,
            max_dex_slot_age:      5,
            min_dex_liquidity_usd: 10_000.0,
            min_price:             0.000_001,
            max_price:             10_000_000.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Stored price types
// ---------------------------------------------------------------------------

/// A CEX price observation.
#[derive(Debug, Clone, Copy)]
pub struct CexPrice {
    /// Mid/last price in USDC (e.g. 86.04 for SOL)
    pub price: f64,
    /// Best bid (for selling on CEX)
    pub bid: f64,
    /// Best ask (for buying on CEX)
    pub ask: f64,
    /// Microsecond timestamp when this was received
    pub updated_at_us: i64,
}

impl CexPrice {
    /// Create from a single mid price (bid = ask = price).
    /// Use when you only have the last trade price, not the full quote.
    pub fn from_mid(price: f64, updated_at_us: i64) -> Self {
        Self { price, bid: price, ask: price, updated_at_us }
    }

    /// Create with explicit bid/ask spread.
    pub fn from_quote(bid: f64, ask: f64, updated_at_us: i64) -> Self {
        Self {
            price: (bid + ask) / 2.0,
            bid,
            ask,
            updated_at_us,
        }
    }

    /// Age of this price in milliseconds.
    #[inline]
    pub fn age_ms(&self, now_us: i64) -> u64 {
        ((now_us - self.updated_at_us).max(0) / 1000) as u64
    }
}

/// A DEX price observation from one pool.
#[derive(Debug, Clone, Copy)]
pub struct DexEntry {
    /// Human price: USDC per SOL (e.g. 86.013)
    pub price: f64,
    /// Pool fee rate as a fraction (e.g. 0.0003)
    pub fee_rate: f64,
    /// Rough USD liquidity depth (sqrt(L) * price)
    pub liquidity_usd: f64,
    /// Slot this was observed at
    pub slot: u64,
    /// Microsecond timestamp when received
    pub received_at_us: i64,
    /// Which DEX this came from
    pub source: DexKind,
}

impl DexEntry {
    /// Age in slots relative to the current known slot.
    #[inline]
    pub fn slot_age(&self, current_slot: u64) -> u64 {
        current_slot.saturating_sub(self.slot)
    }
}

impl From<&PoolPrice> for DexEntry {
    fn from(p: &PoolPrice) -> Self {
        Self {
            price: p.price,
            fee_rate: p.fee_rate,
            liquidity_usd: p.liquidity_usd,
            slot: p.slot,
            received_at_us: p.received_at_us,
            source: p.dex,
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot atomic read of all prices at once
// ---------------------------------------------------------------------------

/// Point-in-time snapshot of all prices, what the spread detector consumes
#[derive(Debug, Clone)]
pub struct PriceSnapshot {
    /// CEX price (Binance)
    pub cex: CexPrice,
    /// Best DEX price (fresher of Orca/Raydium)
    pub best_dex: DexEntry,
    /// Orca price (if available and fresh)
    pub orca: Option<DexEntry>,
    /// Raydium price (if available and fresh)
    pub raydium: Option<DexEntry>,
    /// Latest known Solana slot
    pub current_slot: u64,
    /// Microseconds when this snapshot was taken
    pub taken_at_us: i64,
}

impl PriceSnapshot {
    /// Raw spread: (dex - cex) / cex * 100  (signed, in percent)
    #[inline]
    pub fn spread_pct(&self) -> f64 {
        (self.best_dex.price - self.cex.price) / self.cex.price * 100.0
    }

    /// Absolute spread in percent.
    #[inline]
    pub fn spread_abs_pct(&self) -> f64 {
        self.spread_pct().abs()
    }

    /// Total fees for one round trip (DEX fee + CEX taker fee).
    /// CEX taker fee assumed 0.1% unless overridden.
    #[inline]
    pub fn total_fee_pct(&self, cex_taker_fee_pct: f64) -> f64 {
        self.best_dex.fee_rate * 100.0 + cex_taker_fee_pct
    }

    /// Net profit percent after fees (positive = profitable).
    #[inline]
    pub fn net_profit_pct(&self, cex_taker_fee_pct: f64) -> f64 {
        self.spread_abs_pct() - self.total_fee_pct(cex_taker_fee_pct)
    }

    /// True if the CEX price is higher (DEX is cheaper → buy DEX, sell CEX).
    #[inline]
    pub fn dex_is_cheaper(&self) -> bool {
        self.best_dex.price < self.cex.price
    }
}

impl fmt::Display for PriceSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CEX={:.6} DEX={:.6} ({}) spread={:+.4}% slot={}",
            self.cex.price,
            self.best_dex.price,
            self.best_dex.source,
            self.spread_pct(),
            self.current_slot,
        )
    }
}

// impl fmt::Display for DexKind {
//     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//         match self {
//             DexKind::Orca    => write!(f, "Orca"),
//             DexKind::Raydium => write!(f, "Raydium"),
//         }
//     }
// }

// ---------------------------------------------------------------------------
// PriceState — the main struct
// ---------------------------------------------------------------------------

/// Thread-safe shared price store.
///
/// Wrap in `Arc<PriceState>` and clone to share across tasks.
pub struct PriceState {
    config: PriceStateConfig,

    // ── CEX slot ──────────────────────────────────────────────────────────
    // AtomicCell gives us lock-free read/write of a Copy type.
    // Option<CexPrice> is 32 bytes — fits in AtomicCell on 64-bit platforms.
    cex: AtomicCell<Option<CexPrice>>,

    // ── DEX slots (one per source) ────────────────────────────────────────
    orca:    AtomicCell<Option<DexEntry>>,
    raydium: AtomicCell<Option<DexEntry>>,

    // ── Slot tracker ─────────────────────────────────────────────────────
    // Updated by the Geyser slot update stream.
    // Used to compute DEX price age in slots.
    current_slot: AtomicU64,

    // ── Stats ─────────────────────────────────────────────────────────────
    cex_updates:     AtomicU64,
    dex_updates:     AtomicU64,
    stale_rejections: AtomicU64,
}

impl PriceState {
    /// Create a new [`PriceState`] with the given config.
    pub fn new(config: PriceStateConfig) -> Self {
        Self {
            config,
            cex:              AtomicCell::new(None),
            orca:             AtomicCell::new(None),
            raydium:          AtomicCell::new(None),
            current_slot:     AtomicU64::new(0),
            cex_updates:      AtomicU64::new(0),
            dex_updates:      AtomicU64::new(0),
            stale_rejections: AtomicU64::new(0),
        }
    }

    /// Create with default config.
    pub fn default() -> Self {
        Self::new(PriceStateConfig::default())
    }

    // ── Writers ───────────────────────────────────────────────────────────

    /// Update the CEX price from a Binance WS best-bid/ask update.
    ///
    /// Call this from your Binance WS handler task.
    #[inline]
    pub fn update_cex_quote(&self, bid: f64, ask: f64, updated_at_us: i64) {
        if !self.price_is_sane(bid) || !self.price_is_sane(ask) {
            return;
        }
        self.cex.store(Some(CexPrice::from_quote(bid, ask, updated_at_us)));
        self.cex_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Update the CEX price from a single mid/last price.
    ///
    /// Use this if your Binance WS only gives you last trade price.
    #[inline]
    pub fn update_cex(&self, price: f64, updated_at_us: i64) {
        if !self.price_is_sane(price) {
            return;
        }
        self.cex.store(Some(CexPrice::from_mid(price, updated_at_us)));
        self.cex_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Update a DEX price from a [`PoolPrice`] emitted by `pool_monitor`.
    ///
    /// Call this from your Geyser watch channel consumer task.
    #[inline]
    pub fn update_dex(&self, pool: &PoolPrice) {
        if !self.price_is_sane(pool.price) {
            return;
        }
        if pool.liquidity_usd < self.config.min_dex_liquidity_usd {
            return;
        }

        // ── Cross-DEX sanity check ────────────────────────────────────────
        // If the other DEX has a recent price, reject this update if it
        // deviates by more than 3%. Partial Geyser updates can produce
        // garbage decode values — the other DEX is our ground truth.
        let other_price = match pool.dex {
            DexKind::Orca    => self.raydium.load().map(|e| e.price),
            DexKind::Raydium => self.orca.load().map(|e| e.price),
        };

        if let Some(other) = other_price {
            let deviation = (pool.price - other).abs() / other;
            if deviation > 0.03 {  // 3% max inter-DEX deviation
                // Garbage decode — silently drop
                return;
            }
        }

        let entry = DexEntry::from(pool);

        match pool.dex {
            DexKind::Orca    => self.orca.store(Some(entry)),
            DexKind::Raydium => self.raydium.store(Some(entry)),
        }

        // Keep current_slot up to date from DEX updates too
        self.current_slot.fetch_max(pool.slot, Ordering::Relaxed);
        self.dex_updates.fetch_add(1, Ordering::Relaxed);
    }

    /// Update the current Solana slot.
    ///
    /// Call this from the Geyser slot update handler.
    #[inline]
    pub fn update_slot(&self, slot: u64) {
        self.current_slot.fetch_max(slot, Ordering::Relaxed);
    }

    // ── Readers ───────────────────────────────────────────────────────────

    /// Try to build a [`PriceSnapshot`].
    ///
    /// Returns `None` if:
    /// - CEX price is missing or stale
    /// - All DEX prices are missing or stale
    ///
    /// This is the primary method consumed by `spread_detector`.
    pub fn snapshot(&self) -> Option<PriceSnapshot> {
        let now_us      = now_us();
        let current_slot = self.current_slot.load(Ordering::Relaxed);

        // ── CEX ──────────────────────────────────────────────────────────
        let cex = self.cex.load()?;
        if cex.age_ms(now_us) > self.config.max_cex_age_ms {
            self.stale_rejections.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        // ── DEX ──────────────────────────────────────────────────────────
        let orca    = self.fresh_dex(self.orca.load(),    current_slot);
        let raydium = self.fresh_dex(self.raydium.load(), current_slot);

        // Pick best DEX: prefer fresher slot, fall back to whichever exists
        let best_dex = match (orca, raydium) {
            (Some(o), Some(r)) => {
                // Both fresh — pick the one with more recent slot
                if o.slot >= r.slot { o } else { r }
            }
            (Some(o), None) => o,
            (None, Some(r)) => r,
            (None, None)    => {
                self.stale_rejections.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };

        Some(PriceSnapshot {
            cex,
            best_dex,
            orca,
            raydium,
            current_slot,
            taken_at_us: now_us,
        })
    }

    /// Get the latest CEX price, regardless of staleness.
    /// Returns None if no CEX price has been received yet.
    pub fn cex_raw(&self) -> Option<CexPrice> {
        self.cex.load()
    }

    /// Get the latest Orca price, regardless of staleness.
    pub fn orca_raw(&self) -> Option<DexEntry> {
        self.orca.load()
    }

    /// Get the latest Raydium price, regardless of staleness.
    pub fn raydium_raw(&self) -> Option<DexEntry> {
        self.raydium.load()
    }

    /// Get the best (freshest) DEX price, applying staleness filter.
    pub fn best_dex(&self) -> Option<DexEntry> {
        let slot = self.current_slot.load(Ordering::Relaxed);
        let orca    = self.fresh_dex(self.orca.load(),    slot);
        let raydium = self.fresh_dex(self.raydium.load(), slot);
        match (orca, raydium) {
            (Some(o), Some(r)) => Some(if o.slot >= r.slot { o } else { r }),
            (Some(o), None)    => Some(o),
            (None, Some(r))    => Some(r),
            (None, None)       => None,
        }
    }

    /// Current known Solana slot.
    pub fn current_slot(&self) -> u64 {
        self.current_slot.load(Ordering::Relaxed)
    }

    /// True if CEX price exists and is fresh.
    pub fn cex_is_fresh(&self) -> bool {
        self.cex.load()
            .map(|c| c.age_ms(now_us()) <= self.config.max_cex_age_ms)
            .unwrap_or(false)
    }

    /// True if at least one DEX price is fresh.
    pub fn dex_is_fresh(&self) -> bool {
        self.best_dex().is_some()
    }

    /// True if both CEX and DEX prices are present and fresh.
    pub fn is_ready(&self) -> bool {
        self.cex_is_fresh() && self.dex_is_fresh()
    }

    // ── Stats ─────────────────────────────────────────────────────────────

    /// Diagnostic snapshot — useful for CLI dashboard.
    pub fn stats(&self) -> PriceStateStats {
        PriceStateStats {
            cex_updates:      self.cex_updates.load(Ordering::Relaxed),
            dex_updates:      self.dex_updates.load(Ordering::Relaxed),
            stale_rejections: self.stale_rejections.load(Ordering::Relaxed),
            current_slot:     self.current_slot.load(Ordering::Relaxed),
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────

    /// Apply staleness filter to a loaded DEX entry.
    #[inline]
    fn fresh_dex(&self, entry: Option<DexEntry>, current_slot: u64) -> Option<DexEntry> {
        entry.filter(|e| e.slot_age(current_slot) <= self.config.max_dex_slot_age)
    }

    /// Basic sanity check on a price value.
    #[inline]
    fn price_is_sane(&self, price: f64) -> bool {
        price.is_finite()
            && price >= self.config.min_price
            && price <= self.config.max_price
    }
}

// PriceState is Send + Sync because AtomicCell<T> is Send + Sync for Copy T.
// Verify at compile time:
// const _: () = {
//     fn assert_send_sync<T: Send + Sync>() {}
//     fn check() { assert_send_sync::<PriceState>(); }
// };

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// Diagnostic counters from [`PriceState`].
#[derive(Debug, Clone, Copy)]
pub struct PriceStateStats {
    pub cex_updates:      u64,
    pub dex_updates:      u64,
    pub stale_rejections: u64,
    pub current_slot:     u64,
}

impl fmt::Display for PriceStateStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cex_updates={} dex_updates={} stale_rejections={} slot={}",
            self.cex_updates,
            self.dex_updates,
            self.stale_rejections,
            self.current_slot,
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Current time in microseconds since Unix epoch.
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
    use std::sync::Arc;

    use super::*;
    use crate::dex::pool_monitor::PoolPrice;

    // ── Fixtures ─────────────────────────────────────────────────────────

    fn make_pool_price(dex: DexKind, price: f64, slot: u64) -> PoolPrice {
        PoolPrice {
            pool_address: "testpool".to_string(),
            dex,
            symbol: "SOLUSDC".to_string(),
            price,
            fee_rate: 0.0003,
            liquidity_usd: 500_000.0,
            slot,
            received_at_us: now_us(),
        }
    }

    fn fresh_state() -> PriceState {
        PriceState::new(PriceStateConfig::default())
    }

    // ── CEX updates ───────────────────────────────────────────────────────

    #[test]
    fn test_update_cex_stores_price() {
        let state = fresh_state();
        state.update_cex(86.04, now_us());
        let cex = state.cex_raw().unwrap();
        assert!((cex.price - 86.04).abs() < f64::EPSILON);
    }

    #[test]
    fn test_update_cex_quote_stores_bid_ask() {
        let state = fresh_state();
        state.update_cex_quote(86.00, 86.08, now_us());
        let cex = state.cex_raw().unwrap();
        assert!((cex.bid - 86.00).abs() < f64::EPSILON);
        assert!((cex.ask - 86.08).abs() < f64::EPSILON);
        assert!((cex.price - 86.04).abs() < f64::EPSILON); // mid
    }

    #[test]
    fn test_update_cex_rejects_nan() {
        let state = fresh_state();
        state.update_cex(f64::NAN, now_us());
        assert!(state.cex_raw().is_none());
    }

    #[test]
    fn test_update_cex_rejects_zero() {
        let state = fresh_state();
        state.update_cex(0.0, now_us());
        assert!(state.cex_raw().is_none());
    }

    #[test]
    fn test_update_cex_rejects_negative() {
        let state = fresh_state();
        state.update_cex(-1.0, now_us());
        assert!(state.cex_raw().is_none());
    }

    #[test]
    fn test_update_cex_rejects_inf() {
        let state = fresh_state();
        state.update_cex(f64::INFINITY, now_us());
        assert!(state.cex_raw().is_none());
    }

    #[test]
    fn test_cex_updates_counter() {
        let state = fresh_state();
        state.update_cex(86.0, now_us());
        state.update_cex(86.1, now_us());
        assert_eq!(state.stats().cex_updates, 2);
    }

    // ── DEX updates ───────────────────────────────────────────────────────

    #[test]
    fn test_update_dex_orca() {
        let state = fresh_state();
        let pool = make_pool_price(DexKind::Orca, 86.03, 405_537_094);
        state.update_dex(&pool);
        let entry = state.orca_raw().unwrap();
        assert!((entry.price - 86.03).abs() < f64::EPSILON);
        assert_eq!(entry.source as u8, DexKind::Orca as u8);
    }

    #[test]
    fn test_update_dex_raydium() {
        let state = fresh_state();
        let pool = make_pool_price(DexKind::Raydium, 85.98, 405_537_093);
        state.update_dex(&pool);
        let entry = state.raydium_raw().unwrap();
        assert!((entry.price - 85.98).abs() < f64::EPSILON);
        assert_eq!(entry.source as u8, DexKind::Raydium as u8);
    }

    #[test]
    fn test_update_dex_advances_slot() {
        let state = fresh_state();
        let pool = make_pool_price(DexKind::Orca, 86.0, 405_537_100);
        state.update_dex(&pool);
        assert_eq!(state.current_slot(), 405_537_100);
    }

    #[test]
    fn test_update_dex_rejects_low_liquidity() {
        let state = fresh_state();
        let mut pool = make_pool_price(DexKind::Orca, 86.0, 100);
        pool.liquidity_usd = 100.0; // below 10_000 default
        state.update_dex(&pool);
        assert!(state.orca_raw().is_none());
    }

    #[test]
    fn test_dex_updates_counter() {
        let state = fresh_state();
        state.update_dex(&make_pool_price(DexKind::Orca, 86.0, 100));
        state.update_dex(&make_pool_price(DexKind::Raydium, 85.9, 101));
        assert_eq!(state.stats().dex_updates, 2);
    }

    // ── best_dex picks fresher slot ────────────────────────────────────────

    #[test]
    fn test_best_dex_picks_fresher_slot() {
        let state = fresh_state();
        state.update_slot(405_537_110);

        // Orca is 2 slots old, Raydium is 1 slot old → Raydium wins
        state.update_dex(&make_pool_price(DexKind::Orca,    86.03, 405_537_108));
        state.update_dex(&make_pool_price(DexKind::Raydium, 85.98, 405_537_109));

        let best = state.best_dex().unwrap();
        assert_eq!(best.source as u8, DexKind::Raydium as u8);
        assert_eq!(best.slot, 405_537_109);
    }

    #[test]
    fn test_best_dex_falls_back_to_only_source() {
        let state = fresh_state();
        state.update_slot(405_537_110);
        state.update_dex(&make_pool_price(DexKind::Orca, 86.03, 405_537_108));
        // No Raydium update

        let best = state.best_dex().unwrap();
        assert_eq!(best.source as u8, DexKind::Orca as u8);
    }

    #[test]
    fn test_best_dex_none_when_both_stale() {
        let cfg = PriceStateConfig { max_dex_slot_age: 3, ..Default::default() };
        let state = PriceState::new(cfg);
        state.update_slot(405_537_120); // current slot far ahead

        // Both DEX prices are 15 slots old → stale
        state.update_dex(&make_pool_price(DexKind::Orca,    86.0, 405_537_105));
        state.update_dex(&make_pool_price(DexKind::Raydium, 85.9, 405_537_105));

        assert!(state.best_dex().is_none());
    }

    // ── snapshot ─────────────────────────────────────────────────────────

    #[test]
    fn test_snapshot_returns_none_without_cex() {
        let state = fresh_state();
        state.update_dex(&make_pool_price(DexKind::Orca, 86.0, 100));
        state.update_slot(100);
        assert!(state.snapshot().is_none());
    }

    #[test]
    fn test_snapshot_returns_none_without_dex() {
        let state = fresh_state();
        state.update_cex(86.04, now_us());
        assert!(state.snapshot().is_none());
    }

    #[test]
    fn test_snapshot_success() {
        let state = fresh_state();
        let slot = 405_537_094;
        state.update_cex(86.04, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 86.03, slot));
        state.update_slot(slot);

        let snap = state.snapshot().expect("should have snapshot");
        assert!((snap.cex.price - 86.04).abs() < f64::EPSILON);
        assert!((snap.best_dex.price - 86.03).abs() < f64::EPSILON);
        assert_eq!(snap.current_slot, slot);
    }

    #[test]
    fn test_snapshot_spread_calculation() {
        let state = fresh_state();
        let slot = 100;
        state.update_cex(86.00, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 86.43, slot)); // +0.5%
        state.update_slot(slot);

        let snap = state.snapshot().unwrap();
        let spread = snap.spread_pct();
        // (86.43 - 86.00) / 86.00 * 100 = 0.5%
        assert!((spread - 0.5).abs() < 0.01, "spread={spread:.4}");
    }

    #[test]
    fn test_snapshot_net_profit_after_fees() {
        let state = fresh_state();
        let slot = 100;
        state.update_cex(86.00, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 86.43, slot));
        state.update_slot(slot);

        let snap = state.snapshot().unwrap();
        // spread=0.5%, orca_fee=0.03%, binance_fee=0.1% → net=0.5-0.13=0.37%
        let net = snap.net_profit_pct(0.1);
        assert!(net > 0.0, "should be profitable: net={net:.4}%");
        assert!((net - 0.37).abs() < 0.01, "net={net:.4}%");
    }

    #[test]
    fn test_snapshot_dex_is_cheaper() {
        let state = fresh_state();
        let slot = 100;

        // DEX cheaper → dex_is_cheaper() = true
        state.update_cex(86.00, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 85.57, slot)); // CEX higher
        state.update_slot(slot);
        assert!(state.snapshot().unwrap().dex_is_cheaper());

        // DEX more expensive → dex_is_cheaper() = false
        let state2 = fresh_state();
        state2.update_cex(86.00, now_us());
        state2.update_dex(&make_pool_price(DexKind::Orca, 86.43, slot));
        state2.update_slot(slot);
        assert!(!state2.snapshot().unwrap().dex_is_cheaper());
    }

    // ── staleness ─────────────────────────────────────────────────────────

    #[test]
    fn test_stale_cex_rejected_in_snapshot() {
        let cfg = PriceStateConfig {
            max_cex_age_ms: 1, // 1ms — will be stale immediately
            ..Default::default()
        };
        let state = PriceState::new(cfg);

        // Set CEX price 10ms in the past
        let old_ts = now_us() - 10_000; // 10ms ago
        state.update_cex(86.04, old_ts);
        state.update_dex(&make_pool_price(DexKind::Orca, 86.0, 100));
        state.update_slot(100);

        assert!(state.snapshot().is_none(), "stale CEX should block snapshot");
        assert_eq!(state.stats().stale_rejections, 1);
    }

    #[test]
    fn test_stale_dex_rejected_in_snapshot() {
        let cfg = PriceStateConfig {
            max_dex_slot_age: 2,
            ..Default::default()
        };
        let state = PriceState::new(cfg);
        state.update_cex(86.04, now_us());

        // DEX price is 10 slots old, current is 100 → age=10 > max=2
        state.update_dex(&make_pool_price(DexKind::Orca, 86.0, 90));
        state.update_slot(100);

        assert!(state.snapshot().is_none(), "stale DEX should block snapshot");
    }

    // ── is_ready ──────────────────────────────────────────────────────────

    #[test]
    fn test_is_ready_false_initially() {
        assert!(!fresh_state().is_ready());
    }

    #[test]
    fn test_is_ready_true_with_both_prices() {
        let state = fresh_state();
        state.update_cex(86.0, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 86.0, 100));
        state.update_slot(100);
        assert!(state.is_ready());
    }

    // ── Display ───────────────────────────────────────────────────────────

    #[test]
    fn test_snapshot_display() {
        let state = fresh_state();
        state.update_cex(86.04, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 86.03, 405_537_094));
        state.update_slot(405_537_094);
        let snap = state.snapshot().unwrap();
        let s = format!("{snap}");
        assert!(s.contains("86.040000"), "missing cex price: {s}");
        assert!(s.contains("Orca"),      "missing dex source: {s}");
        println!("{s}");
    }

    #[test]
    fn test_stats_display() {
        let state = fresh_state();
        state.update_cex(86.0, now_us());
        state.update_dex(&make_pool_price(DexKind::Orca, 86.0, 100));
        let s = format!("{}", state.stats());
        assert!(s.contains("cex_updates=1"));
        assert!(s.contains("dex_updates=1"));
        println!("{s}");
    }

    // ── Arc sharing ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_arc_shared_across_tasks() {
        let state = Arc::new(fresh_state());
        let writer = state.clone();
        let reader = state.clone();

        let write_task = tokio::spawn(async move {
            writer.update_cex(86.04, now_us());
            writer.update_dex(&make_pool_price(DexKind::Orca, 86.03, 405_537_094));
            writer.update_slot(405_537_094);
        });

        write_task.await.unwrap();

        // Reader sees the update
        assert!(reader.snapshot().is_some());
        assert!((reader.cex_raw().unwrap().price - 86.04).abs() < f64::EPSILON);
    }
}