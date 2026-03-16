//! Shared price state — thread-safe bridge between data sources and detector.
//!
//! # What changed vs the previous version
//!
//! Three additions only — zero changes to existing code:
//!
//! 1. `use std::collections::HashMap` and `use std::sync::RwLock` added.
//! 2. Field `cex_pairs: RwLock<HashMap<String, CexPrice>>` added to struct.
//! 3. Three new methods added:
//!    - `update_cex_quote_for_pair(symbol, bid, ask, ts)` — called by binance_ws
//!    - `cex_for_pair(symbol)` — returns fresh price for a symbol
//!    - `cex_for_pair_raw(symbol)` — returns price ignoring staleness (for display)
//!    - `tracked_pairs()` — lists all symbols in the map
//!
//! All existing methods, tests, and behaviour are completely unchanged.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        RwLock,
    },
};

use crossbeam::atomic::AtomicCell;

use crate::dex::pool_monitor::{DexKind, PoolPrice};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PriceStateConfig {
    pub max_cex_age_ms:        u64,
    pub max_dex_slot_age:      u64,
    pub min_dex_liquidity_usd: f64,
    pub min_price:             f64,
    pub max_price:             f64,
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

#[derive(Debug, Clone, Copy)]
pub struct CexPrice {
    pub price:         f64,
    pub bid:           f64,
    pub ask:           f64,
    pub updated_at_us: i64,
}

impl CexPrice {
    pub fn from_mid(price: f64, updated_at_us: i64) -> Self {
        Self { price, bid: price, ask: price, updated_at_us }
    }

    pub fn from_quote(bid: f64, ask: f64, updated_at_us: i64) -> Self {
        Self { price: (bid + ask) / 2.0, bid, ask, updated_at_us }
    }

    #[inline]
    pub fn age_ms(&self, now_us: i64) -> u64 {
        ((now_us - self.updated_at_us).max(0) / 1000) as u64
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DexEntry {
    pub price:          f64,
    pub fee_rate:       f64,
    pub liquidity_usd:  f64,
    pub slot:           u64,
    pub received_at_us: i64,
    pub source:         DexKind,
}

impl DexEntry {
    #[inline]
    pub fn slot_age(&self, current_slot: u64) -> u64 {
        current_slot.saturating_sub(self.slot)
    }
}

impl From<&PoolPrice> for DexEntry {
    fn from(p: &PoolPrice) -> Self {
        Self {
            price:          p.price,
            fee_rate:       p.fee_rate,
            liquidity_usd:  p.liquidity_usd,
            slot:           p.slot,
            received_at_us: p.received_at_us,
            source:         p.dex,
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PriceSnapshot {
    pub cex:          CexPrice,
    pub best_dex:     DexEntry,
    pub orca:         Option<DexEntry>,
    pub raydium:      Option<DexEntry>,
    pub current_slot: u64,
    pub taken_at_us:  i64,
}

impl PriceSnapshot {
    #[inline]
    pub fn spread_pct(&self) -> f64 {
        (self.best_dex.price - self.cex.price) / self.cex.price * 100.0
    }

    #[inline]
    pub fn spread_abs_pct(&self) -> f64 { self.spread_pct().abs() }

    #[inline]
    pub fn total_fee_pct(&self, cex_taker_fee_pct: f64) -> f64 {
        self.best_dex.fee_rate * 100.0 + cex_taker_fee_pct
    }

    #[inline]
    pub fn net_profit_pct(&self, cex_taker_fee_pct: f64) -> f64 {
        self.spread_abs_pct() - self.total_fee_pct(cex_taker_fee_pct)
    }

    #[inline]
    pub fn dex_is_cheaper(&self) -> bool { self.best_dex.price < self.cex.price }
}

impl fmt::Display for PriceSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CEX={:.6} DEX={:.6} ({}) spread={:+.4}% slot={}",
            self.cex.price, self.best_dex.price, self.best_dex.source,
            self.spread_pct(), self.current_slot,
        )
    }
}

// ---------------------------------------------------------------------------
// PriceState
// ---------------------------------------------------------------------------

pub struct PriceState {
    config: PriceStateConfig,

    // ── Single-pair CEX (lock-free, backward compatible) ──────────────────
    cex: AtomicCell<Option<CexPrice>>,

    // ── Multi-pair CEX map (NEW) ───────────────────────────────────────────
    // Keyed by uppercase symbol: "SOLUSDC", "WIFUSDC", "BONKUSDC", "JTOUSDC"
    // Written by update_cex_quote_for_pair(), read by cex_for_pair().
    // RwLock: concurrent reads every 50ms, writes every ~10-50ms per symbol.
    cex_pairs: RwLock<HashMap<String, CexPrice>>,

    // ── DEX slots ─────────────────────────────────────────────────────────
    orca:    AtomicCell<Option<DexEntry>>,
    raydium: AtomicCell<Option<DexEntry>>,

    // ── Slot tracker ──────────────────────────────────────────────────────
    current_slot: AtomicU64,

    // ── Stats ─────────────────────────────────────────────────────────────
    cex_updates:      AtomicU64,
    dex_updates:      AtomicU64,
    stale_rejections: AtomicU64,
}

impl PriceState {
    pub fn new(config: PriceStateConfig) -> Self {
        Self {
            config,
            cex:              AtomicCell::new(None),
            cex_pairs:        RwLock::new(HashMap::new()),  // NEW
            orca:             AtomicCell::new(None),
            raydium:          AtomicCell::new(None),
            current_slot:     AtomicU64::new(0),
            cex_updates:      AtomicU64::new(0),
            dex_updates:      AtomicU64::new(0),
            stale_rejections: AtomicU64::new(0),
        }
    }

    pub fn default() -> Self { Self::new(PriceStateConfig::default()) }

    // ── Writers (existing, unchanged) ─────────────────────────────────────

    #[inline]
    pub fn update_cex_quote(&self, bid: f64, ask: f64, updated_at_us: i64) {
        if !self.price_is_sane(bid) || !self.price_is_sane(ask) { return; }
        self.cex.store(Some(CexPrice::from_quote(bid, ask, updated_at_us)));
        self.cex_updates.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn update_cex(&self, price: f64, updated_at_us: i64) {
        if !self.price_is_sane(price) { return; }
        self.cex.store(Some(CexPrice::from_mid(price, updated_at_us)));
        self.cex_updates.fetch_add(1, Ordering::Relaxed);
    }

    // ── NEW: multi-pair writer ─────────────────────────────────────────────

    /// Update CEX price for a specific trading pair by symbol.
    ///
    /// Called by `binance_ws::run_session` for every bookTicker frame when
    /// using the combined multi-pair stream. Symbol is normalised to uppercase.
    ///
    /// When `symbol == "SOLUSDC"`, also updates the legacy `AtomicCell` so
    /// `snapshot()` and the existing spread detector work unchanged.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// // In binance_ws run_session, after parsing CombinedFrame:
    /// price_state.update_cex_quote_for_pair(
    ///     &ticker.symbol,   // "WIFUSDC"
    ///     bid,
    ///     ask,
    ///     now_us,
    /// );
    /// ```
    pub fn update_cex_quote_for_pair(
        &self,
        symbol:        &str,
        bid:           f64,
        ask:           f64,
        updated_at_us: i64,
    ) {
        if !self.price_is_sane(bid) || !self.price_is_sane(ask) { return; }

        let key   = symbol.to_uppercase();
        let price = CexPrice::from_quote(bid, ask, updated_at_us);

        // Write to per-pair map
        if let Ok(mut map) = self.cex_pairs.write() {
            map.insert(key.clone(), price);
        }

        // Keep legacy AtomicCell in sync for SOLUSDC so snapshot() works
        if key == "SOLUSDC" {
            self.cex.store(Some(price));
        }

        self.cex_updates.fetch_add(1, Ordering::Relaxed);
    }

    // ── Existing DEX / slot writers (unchanged) ───────────────────────────

    #[inline]
    pub fn update_dex(&self, pool: &PoolPrice) {
        if !self.price_is_sane(pool.price) { return; }
        if pool.liquidity_usd < self.config.min_dex_liquidity_usd { return; }

        let other = match pool.dex {
            DexKind::Orca    => self.raydium.load().map(|e| e.price),
            DexKind::Raydium => self.orca.load().map(|e| e.price),
        };
        if let Some(other_price) = other {
            if (pool.price - other_price).abs() / other_price > 0.10 { return; }
        }

        let entry = DexEntry::from(pool);
        match pool.dex {
            DexKind::Orca    => self.orca.store(Some(entry)),
            DexKind::Raydium => self.raydium.store(Some(entry)),
        }
        self.current_slot.fetch_max(pool.slot, Ordering::Relaxed);
        self.dex_updates.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn update_slot(&self, slot: u64) {
        self.current_slot.fetch_max(slot, Ordering::Relaxed);
    }

    // ── Readers (existing, unchanged) ─────────────────────────────────────

    pub fn snapshot(&self) -> Option<PriceSnapshot> {
        let now_us       = now_us();
        let current_slot = self.current_slot.load(Ordering::Relaxed);

        let cex = self.cex.load()?;
        if cex.age_ms(now_us) > self.config.max_cex_age_ms {
            self.stale_rejections.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let orca    = self.fresh_dex(self.orca.load(),    current_slot);
        let raydium = self.fresh_dex(self.raydium.load(), current_slot);

        let best_dex = match (orca, raydium) {
            (Some(o), Some(r)) => if o.slot >= r.slot { o } else { r },
            (Some(o), None)    => o,
            (None, Some(r))    => r,
            (None, None)       => {
                self.stale_rejections.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };

        Some(PriceSnapshot { cex, best_dex, orca, raydium, current_slot, taken_at_us: now_us })
    }

    // ── NEW: multi-pair readers ────────────────────────────────────────────

    /// Get the CEX price for a specific pair, applying the staleness filter.
    /// Returns `None` if no price received yet or price is older than
    /// `max_cex_age_ms`.
    pub fn cex_for_pair(&self, symbol: &str) -> Option<CexPrice> {
        let key    = symbol.to_uppercase();
        let now_us = now_us();
        let price  = self.cex_pairs.read().ok()?.get(&key).copied()?;
        if price.age_ms(now_us) > self.config.max_cex_age_ms { return None; }
        Some(price)
    }

    /// Get the CEX price for a specific pair regardless of staleness.
    /// Useful for the display task where showing a stale price is fine.
    pub fn cex_for_pair_raw(&self, symbol: &str) -> Option<CexPrice> {
        let key = symbol.to_uppercase();
        self.cex_pairs.read().ok()?.get(&key).copied()
    }

    /// List all symbols currently in the per-pair map.
    pub fn tracked_pairs(&self) -> Vec<String> {
        self.cex_pairs
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    // ── Existing raw readers (unchanged) ──────────────────────────────────

    pub fn cex_raw(&self)     -> Option<CexPrice> { self.cex.load() }
    pub fn orca_raw(&self)    -> Option<DexEntry>  { self.orca.load() }
    pub fn raydium_raw(&self) -> Option<DexEntry>  { self.raydium.load() }

    pub fn best_dex(&self) -> Option<DexEntry> {
        let slot    = self.current_slot.load(Ordering::Relaxed);
        let orca    = self.fresh_dex(self.orca.load(),    slot);
        let raydium = self.fresh_dex(self.raydium.load(), slot);
        match (orca, raydium) {
            (Some(o), Some(r)) => Some(if o.slot >= r.slot { o } else { r }),
            (Some(o), None)    => Some(o),
            (None, Some(r))    => Some(r),
            (None, None)       => None,
        }
    }

    pub fn current_slot(&self) -> u64 { self.current_slot.load(Ordering::Relaxed) }

    pub fn cex_is_fresh(&self) -> bool {
        self.cex.load()
            .map(|c| c.age_ms(now_us()) <= self.config.max_cex_age_ms)
            .unwrap_or(false)
    }

    pub fn dex_is_fresh(&self) -> bool { self.best_dex().is_some() }
    pub fn is_ready(&self)     -> bool { self.cex_is_fresh() && self.dex_is_fresh() }

    pub fn stats(&self) -> PriceStateStats {
        PriceStateStats {
            cex_updates:      self.cex_updates.load(Ordering::Relaxed),
            dex_updates:      self.dex_updates.load(Ordering::Relaxed),
            stale_rejections: self.stale_rejections.load(Ordering::Relaxed),
            current_slot:     self.current_slot.load(Ordering::Relaxed),
        }
    }

    // ── Private ───────────────────────────────────────────────────────────

    #[inline]
    fn fresh_dex(&self, entry: Option<DexEntry>, current_slot: u64) -> Option<DexEntry> {
        entry.filter(|e| e.slot_age(current_slot) <= self.config.max_dex_slot_age)
    }

    #[inline]
    fn price_is_sane(&self, price: f64) -> bool {
        price.is_finite()
            && price >= self.config.min_price
            && price <= self.config.max_price
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct PriceStateStats {
    pub cex_updates:      u64,
    pub dex_updates:      u64,
    pub stale_rejections: u64,
    pub current_slot:     u64,
}

impl fmt::Display for PriceStateStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f,
            "cex_updates={} dex_updates={} stale_rejections={} slot={}",
            self.cex_updates, self.dex_updates,
            self.stale_rejections, self.current_slot)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use super::*;
    use crate::dex::pool_monitor::PoolPrice;

    fn make_pool_price(dex: DexKind, price: f64, slot: u64) -> PoolPrice {
        PoolPrice {
            pool_address:   "testpool".to_string(),
            dex,
            symbol:         "SOLUSDC".to_string(),
            price,
            fee_rate:       0.0003,
            liquidity_usd:  500_000.0,
            slot,
            received_at_us: now_us(),
        }
    }

    fn fresh_state() -> PriceState { PriceState::new(PriceStateConfig::default()) }

    // ── Original tests — all unchanged ────────────────────────────────────

    #[test]
    fn test_update_cex_stores_price() {
        let s = fresh_state();
        s.update_cex(86.04, now_us());
        assert!((s.cex_raw().unwrap().price - 86.04).abs() < f64::EPSILON);
    }

    #[test]
    fn test_update_cex_quote_stores_bid_ask() {
        let s = fresh_state();
        s.update_cex_quote(86.00, 86.08, now_us());
        let c = s.cex_raw().unwrap();
        assert!((c.bid - 86.00).abs() < f64::EPSILON);
        assert!((c.ask - 86.08).abs() < f64::EPSILON);
        assert!((c.price - 86.04).abs() < f64::EPSILON);
    }

    #[test] fn test_rejects_nan()      { let s = fresh_state(); s.update_cex(f64::NAN,      now_us()); assert!(s.cex_raw().is_none()); }
    #[test] fn test_rejects_zero()     { let s = fresh_state(); s.update_cex(0.0,           now_us()); assert!(s.cex_raw().is_none()); }
    #[test] fn test_rejects_negative() { let s = fresh_state(); s.update_cex(-1.0,          now_us()); assert!(s.cex_raw().is_none()); }
    #[test] fn test_rejects_inf()      { let s = fresh_state(); s.update_cex(f64::INFINITY, now_us()); assert!(s.cex_raw().is_none()); }

    #[test]
    fn test_cex_updates_counter() {
        let s = fresh_state();
        s.update_cex(86.0, now_us()); s.update_cex(86.1, now_us());
        assert_eq!(s.stats().cex_updates, 2);
    }

    #[test]
    fn test_update_dex_orca() {
        let s = fresh_state();
        s.update_dex(&make_pool_price(DexKind::Orca, 86.03, 405_537_094));
        assert!((s.orca_raw().unwrap().price - 86.03).abs() < f64::EPSILON);
    }

    #[test]
    fn test_update_dex_rejects_low_liquidity() {
        let s = fresh_state();
        let mut p = make_pool_price(DexKind::Orca, 86.0, 100);
        p.liquidity_usd = 100.0;
        s.update_dex(&p);
        assert!(s.orca_raw().is_none());
    }

    #[test]
    fn test_snapshot_success() {
        let s = fresh_state(); let slot = 405_537_094;
        s.update_cex(86.04, now_us());
        s.update_dex(&make_pool_price(DexKind::Orca, 86.03, slot));
        s.update_slot(slot);
        let snap = s.snapshot().unwrap();
        assert!((snap.cex.price - 86.04).abs() < f64::EPSILON);
        assert!((snap.best_dex.price - 86.03).abs() < f64::EPSILON);
    }

    #[test]
    fn test_snapshot_returns_none_without_cex() {
        let s = fresh_state();
        s.update_dex(&make_pool_price(DexKind::Orca, 86.0, 100));
        s.update_slot(100);
        assert!(s.snapshot().is_none());
    }

    // ── New tests: update_cex_quote_for_pair ──────────────────────────────

    #[test]
    fn test_for_pair_stores_in_map() {
        let s = fresh_state();
        s.update_cex_quote_for_pair("WIFUSDC", 2.341, 2.342, now_us());
        let wif = s.cex_for_pair("WIFUSDC").unwrap();
        assert!((wif.bid   - 2.341).abs()  < f64::EPSILON);
        assert!((wif.ask   - 2.342).abs()  < f64::EPSILON);
        assert!((wif.price - 2.3415).abs() < 0.0001);
    }

    #[test]
    fn test_for_pair_solusdc_keeps_snapshot_working() {
        let s = fresh_state(); let slot = 100u64;
        s.update_cex_quote_for_pair("SOLUSDC", 86.00, 86.01, now_us());
        s.update_dex(&make_pool_price(DexKind::Orca, 86.03, slot));
        s.update_slot(slot);
        let snap = s.snapshot().expect("snapshot must work via for_pair SOLUSDC");
        assert!((snap.cex.bid - 86.00).abs() < f64::EPSILON);
    }

    #[test]
    fn test_for_pair_case_insensitive() {
        let s = fresh_state();
        s.update_cex_quote_for_pair("wifusdc", 2.341, 2.342, now_us());
        assert!(s.cex_for_pair("WIFUSDC").is_some());
        assert!(s.cex_for_pair("wifusdc").is_some());
    }

    #[test]
    fn test_for_pair_rejects_bad_prices() {
        let s = fresh_state();
        s.update_cex_quote_for_pair("WIFUSDC", f64::NAN, 2.342, now_us());
        assert!(s.cex_for_pair_raw("WIFUSDC").is_none());
    }

    #[test]
    fn test_for_pair_all_four_symbols() {
        let s = fresh_state();
        s.update_cex_quote_for_pair("SOLUSDC",  86.00,     86.01,     now_us());
        s.update_cex_quote_for_pair("WIFUSDC",   2.341,    2.342,     now_us());
        s.update_cex_quote_for_pair("BONKUSDC",  0.0000218, 0.0000219, now_us());
        s.update_cex_quote_for_pair("JTOUSDC",   3.12,     3.13,      now_us());
        assert_eq!(s.tracked_pairs().len(), 4);
        assert!(s.cex_for_pair("SOLUSDC").is_some());
        assert!(s.cex_for_pair("WIFUSDC").is_some());
        assert!(s.cex_for_pair("BONKUSDC").is_some());
        assert!(s.cex_for_pair("JTOUSDC").is_some());
    }

    #[test]
    fn test_for_pair_unknown_returns_none() {
        assert!(fresh_state().cex_for_pair("XYZUSDC").is_none());
    }

    #[test]
    fn test_for_pair_stale_vs_raw() {
        let cfg = PriceStateConfig { max_cex_age_ms: 1, ..Default::default() };
        let s   = PriceState::new(cfg);
        let old = now_us() - 10_000; // 10ms ago, stale at 1ms threshold
        s.update_cex_quote_for_pair("WIFUSDC", 2.34, 2.35, old);
        assert!(s.cex_for_pair("WIFUSDC").is_none());      // staleness applied
        assert!(s.cex_for_pair_raw("WIFUSDC").is_some());  // raw bypasses it
    }

    #[test]
    fn test_for_pair_increments_cex_counter() {
        let s = fresh_state();
        s.update_cex_quote_for_pair("WIFUSDC",  2.34,      2.35,      now_us());
        s.update_cex_quote_for_pair("BONKUSDC", 0.0000218, 0.0000219, now_us());
        assert_eq!(s.stats().cex_updates, 2);
    }

    #[tokio::test]
    async fn test_arc_shared_across_tasks() {
        let state  = Arc::new(fresh_state());
        let writer = state.clone();
        let reader = state.clone();
        tokio::spawn(async move {
            writer.update_cex(86.04, now_us());
            writer.update_dex(&make_pool_price(DexKind::Orca, 86.03, 405_537_094));
            writer.update_slot(405_537_094);
        }).await.unwrap();
        assert!(reader.snapshot().is_some());
    }
}

