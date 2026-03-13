//! Paper trading demo — live CLI dashboard.
//!
//! Wires the full pipeline end-to-end against real Geyser DEX prices
//! and a simulated Binance CEX price feed.
//!
//! # Usage
//!
//! ```bash
//! GEYSER_ENDPOINT="https://solana-rpc.parafi.tech:10443" \
//! GEYSER_X_TOKEN="your-token" \
//! cargo run --bin paper_trade_demo
//! ```
//!
//! # Optional env vars
//!
//! | Var                    | Default   | Description                        |
//! |------------------------|-----------|------------------------------------|
//! | `INITIAL_BALANCE`      | 10000.0   | Starting USDC balance              |
//! | `TRADE_SIZE`           | 1000.0    | Notional per trade                 |
//! | `MIN_NET_SPREAD_PCT`   | 0.15      | Signal threshold (% after fees)    |
//! | `CEX_VOLATILITY`       | 0.0015    | Fake Binance random walk amplitude |
//! | `SPIKE_PROBABILITY`    | 0.01      | Probability of a price spike/tick  |
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────────┐     watch::Receiver     ┌─────────────────┐
//!  │ pool_monitor │ ──────────────────────▶ │  geyser_task    │
//!  └──────────────┘                         │  update_dex()   │
//!                                           └────────┬────────┘
//!  ┌──────────────┐                                  │
//!  │ binance_task │ ── update_cex() ─────────────────┤
//!  │ (simulated)  │                                  │
//!  └──────────────┘                         ┌────────▼────────┐
//!                                           │   PriceState    │
//!                                           └────────┬────────┘
//!                                                    │ snapshot()
//!                                           ┌────────▼────────┐
//!                                           │ detector_task   │
//!                                           │ 50ms tick       │
//!                                           └────────┬────────┘
//!                                                    │ on_signal()
//!                                           ┌────────▼────────┐
//!                                           │  PaperEngine    │
//!                                           │  PnlTracker     │
//!                                           └────────┬────────┘
//!                                                    │
//!                                           ┌────────▼────────┐
//!                                           │ display_task    │
//!                                           │ 1s redraw       │
//!                                           └─────────────────┘
//! ```

use std::{
    collections::VecDeque,
    env,
    sync::Arc,
    time::Duration,
};

use tokio::{
    signal,
    sync::Mutex,
    time::{interval, sleep},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use pnl::{
    dex::pool_monitor::{PoolMonitorConfig, start_pool_monitor},
    paper::{
        paper_engine::{PaperEngine, PaperEngineConfig, SkipReason, TradeRecord},
        pnl_tracker::PnlTracker,
    },
    strategy::{
        price_state::{PriceState, PriceStateConfig},
        spread_detector::{SpreadConfig, SpreadDetector, SpreadSignal},
    },
};

// ---------------------------------------------------------------------------
// Config from environment
// ---------------------------------------------------------------------------

struct DemoConfig {
    initial_balance:    f64,
    trade_size:         f64,
    min_net_spread_pct: f64,
    max_net_spread_pct: f64,
    cex_volatility:     f64,   // random walk amplitude per 100ms tick
    spike_probability:  f64,   // probability of injecting a 0.5% spike
    spike_magnitude:    f64,   // spike size as a fraction
}

impl DemoConfig {
    fn from_env() -> Self {
        Self {
            initial_balance:    env_f64("INITIAL_BALANCE",    10_000.0),
            trade_size:         env_f64("TRADE_SIZE",          1_000.0),
            min_net_spread_pct: env_f64("MIN_NET_SPREAD_PCT",     0.15),
            max_net_spread_pct: env_f64("MIN_NET_SPREAD_PCT",     2.0),
            cex_volatility:     env_f64("CEX_VOLATILITY",        0.0015),
            spike_probability:  env_f64("SPIKE_PROBABILITY",      0.01),
            spike_magnitude:    env_f64("SPIKE_MAGNITUDE",        0.005),
        }
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Shared display state
// ---------------------------------------------------------------------------

struct DisplayState {
    signal_log: VecDeque<SpreadSignal>,
    trade_log:  VecDeque<TradeRecord>,
    skip_counts: SkipCounts,
}

#[derive(Default)]
struct SkipCounts {
    trade_open:    u64,
    no_balance:    u64,
    neg_fills:     u64,
    no_snapshot:   u64,
}

impl DisplayState {
    fn new() -> Self {
        Self {
            signal_log:  VecDeque::with_capacity(10),
            trade_log:   VecDeque::with_capacity(10),
            skip_counts: SkipCounts::default(),
        }
    }

    fn push_signal(&mut self, sig: SpreadSignal) {
        if self.signal_log.len() >= 5 { self.signal_log.pop_front(); }
        self.signal_log.push_back(sig);
    }

    fn push_trade(&mut self, rec: TradeRecord) {
        if self.trade_log.len() >= 5 { self.trade_log.pop_front(); }
        self.trade_log.push_back(rec);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG")
                .unwrap_or_else(|_| "warn,paper_trade_demo=info".into())
        )
        .with_target(false)
        .init();

    let cfg = DemoConfig::from_env();

    info!("Starting paper trade demo");
    info!(
        "Config: balance=${} trade=${} min_spread={:.2}%",
        cfg.initial_balance, cfg.trade_size, cfg.min_net_spread_pct
    );

    // ── Shared state ──────────────────────────────────────────────────────
    let price_state = Arc::new(PriceState::new(PriceStateConfig::default()));

    let detector = Arc::new(SpreadDetector::new(SpreadConfig {
        min_net_spread_pct:     cfg.min_net_spread_pct,
        max_spread_pct:         cfg.max_net_spread_pct,
        trade_size_usdt:        cfg.trade_size,
        cex_taker_fee_pct:      0.10,
        min_pool_liquidity_usd: 50_000.0,
        cooldown_ms:            500,
        max_signal_slot_age:    3,
        min_net_profit_usdt:    0.05,
    }));

    let engine = Arc::new(Mutex::new(PaperEngine::new(PaperEngineConfig {
        initial_balance_usdt:  cfg.initial_balance,
        trade_size_usdt:       cfg.trade_size,
        cex_taker_fee:         0.001,
        cex_spread_fraction:   0.0001,
        max_concurrent_trades: 1,
        check_balance:         true,
    })));

    let tracker = Arc::new(Mutex::new(PnlTracker::new(cfg.initial_balance)));

    let display_state = Arc::new(Mutex::new(DisplayState::new()));

    let cancel = CancellationToken::new();

    // ── Pool monitor ──────────────────────────────────────────────────────
    let pool_cfg = PoolMonitorConfig::from_env()?;
    let pool_count = pool_cfg.pools.len();
    let (_handle, pool_rx) = start_pool_monitor(pool_cfg).await?;

    info!("Pool monitor started with {} pools", pool_count);

    // ── Launch tasks ──────────────────────────────────────────────────────
    let t1 = tokio::spawn(geyser_task(
        pool_rx,
        Arc::clone(&price_state),
        cancel.clone(),
    ));

    let t2 = tokio::spawn(binance_task(
        Arc::clone(&price_state),
        cfg.cex_volatility,
        cfg.spike_probability,
        cfg.spike_magnitude,
        cancel.clone(),
    ));

    let t3 = tokio::spawn(detector_task(
        Arc::clone(&price_state),
        Arc::clone(&detector),
        Arc::clone(&engine),
        Arc::clone(&tracker),
        Arc::clone(&display_state),
        cancel.clone(),
    ));

    let t4 = tokio::spawn(display_task(
        Arc::clone(&price_state),
        Arc::clone(&detector),
        Arc::clone(&tracker),
        Arc::clone(&display_state),
        cancel.clone(),
    ));

    // ── Wait for Ctrl+C ───────────────────────────────────────────────────
    signal::ctrl_c().await?;
    println!("\n\nShutting down...");
    cancel.cancel();

    // Give tasks 2s to finish cleanly
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = tokio::join!(t1, t2, t3, t4);
    }).await;

    // ── Final stats ───────────────────────────────────────────────────────
    println!("\n{}", "═".repeat(55));
    println!("  FINAL SESSION STATS");
    println!("{}", "═".repeat(55));
    let snap = tracker.lock().await.snapshot();
    println!("{snap}");
    println!("{}", "═".repeat(55));

    Ok(())
}

// ---------------------------------------------------------------------------
// Task 1: Geyser → PriceState
// ---------------------------------------------------------------------------

async fn geyser_task(
    mut rx: tokio::sync::watch::Receiver<Option<pnl::dex::pool_monitor::PoolPrice>>,
    price_state: Arc<PriceState>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = rx.changed() => {
                if result.is_err() { break; }
                let pool = rx.borrow().clone();
                if let Some(p) = pool {
                    price_state.update_dex(&p);
                    price_state.update_slot(p.slot);
                }
            }
        }
    }
    info!("geyser_task stopped");
}

// ---------------------------------------------------------------------------
// Task 2: Simulated Binance feed → PriceState
// ---------------------------------------------------------------------------
//
// Random walk around the current DEX mid price.
// When the DEX lags a spike, the spread widens and signals fire.

async fn binance_task(
    price_state: Arc<PriceState>,
    volatility: f64,
    spike_prob: f64,
    spike_mag: f64,
    cancel: CancellationToken,
) {
    // Start from a reasonable SOL price; will track DEX mid once data arrives
    let mut cex_price = 86.00_f64;
    let mut tick_interval = interval(Duration::from_millis(100));

    // Wait until DEX prices arrive before starting
    info!("binance_task: waiting for DEX prices before starting...");
    loop {
        if price_state.dex_is_fresh() { break; }
        if cancel.is_cancelled() { return; }
        sleep(Duration::from_millis(200)).await;
    }

    // Seed CEX price from first DEX price
    if let Some(dex) = price_state.best_dex() {
        cex_price = dex.price;
        info!("binance_task: seeded CEX price from DEX: ${:.4}", cex_price);
    }

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick_interval.tick() => {
                // Random walk using LCG (no rand crate needed)
                let r1 = fast_rand();
                let r2 = fast_rand();

                // Gaussian-ish: sum of two uniforms, centered at 0
                let walk = (r1 + r2 - 1.0) * volatility * cex_price;

                // Occasional spike: 1% chance each tick
                let spike = if fast_rand() < spike_prob {
                    let dir = if fast_rand() > 0.5 { 1.0 } else { -1.0 };
                    dir * spike_mag * cex_price
                } else {
                    0.0
                };

                cex_price += walk + spike;

                // Clamp to ±5% of current DEX mid to avoid runaway
                if let Some(dex) = price_state.best_dex() {
                    let mid = dex.price;
                    cex_price = cex_price.clamp(mid * 0.95, mid * 1.05);
                }

                let now_us = now_us();
                price_state.update_cex(cex_price, now_us);
            }
        }
    }
    info!("binance_task stopped");
}

// ---------------------------------------------------------------------------
// Task 3: Detector tick → Engine → Tracker
// ---------------------------------------------------------------------------

async fn detector_task(
    price_state: Arc<PriceState>,
    detector:    Arc<SpreadDetector>,
    engine:      Arc<Mutex<PaperEngine>>,
    tracker:     Arc<Mutex<PnlTracker>>,
    display:     Arc<Mutex<DisplayState>>,
    cancel:      CancellationToken,
) {
    let mut tick = interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                // Get snapshot — None if prices are stale/missing
                let snap = match price_state.snapshot() {
                    Some(s) => s,
                    None    => {
                        display.lock().await.skip_counts.no_snapshot += 1;
                        continue;
                    }
                };

                // Check for spread signal
                let signal = match detector.check(&snap) {
                    Some(s) => s,
                    None    => continue,
                };

                // Log signal before trying to trade it
                display.lock().await.push_signal(signal.clone());

                // Try to paper trade
                let result = engine.lock().await.on_signal(signal);

                match result {
                    Ok(record) => {
                        // Record in tracker
                        tracker.lock().await.record(&record);
                        // Log trade
                        display.lock().await.push_trade(record);
                    }
                    Err(SkipReason::TradeAlreadyOpen) => {
                        display.lock().await.skip_counts.trade_open += 1;
                    }
                    Err(SkipReason::InsufficientBalance) => {
                        display.lock().await.skip_counts.no_balance += 1;
                        warn!("Insufficient balance — consider reducing trade size");
                    }
                    Err(SkipReason::NegativeAfterFills) => {
                        display.lock().await.skip_counts.neg_fills += 1;
                    }
                }
            }
        }
    }
    info!("detector_task stopped");
}

// ---------------------------------------------------------------------------
// Task 4: Display (redraws every 1 second)
// ---------------------------------------------------------------------------

async fn display_task(
    price_state: Arc<PriceState>,
    detector:    Arc<SpreadDetector>,
    tracker:     Arc<Mutex<PnlTracker>>,
    display:     Arc<Mutex<DisplayState>>,
    cancel:      CancellationToken,
) {
    let mut tick = interval(Duration::from_secs(1));
    let mut frame: u64 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                frame += 1;
                render(frame, &price_state, &detector, &tracker, &display).await;
            }
        }
    }
}

async fn render(
    frame: u64,
    price_state: &Arc<PriceState>,
    detector:    &Arc<SpreadDetector>,
    tracker:     &Arc<Mutex<PnlTracker>>,
    display:     &Arc<Mutex<DisplayState>>,
) {
    // Clear screen and move cursor to top-left
    print!("\x1b[2J\x1b[H");

    let w = 57usize; // box width including borders

    // ── Header ────────────────────────────────────────────────────────────
    println!("{}",   "═".repeat(w));
    println!("  🤖  Paper Trade  │  frame={}  │  Ctrl+C to stop", frame);
    println!("{}",   "═".repeat(w));

    // ── Live prices ───────────────────────────────────────────────────────
    println!("┌─ Live Prices {}", "─".repeat(w - 15));

    match price_state.cex_raw() {
        Some(c) => println!("│  CEX (Binance):  ${:.6}", c.price),
        None    => println!("│  CEX (Binance):  waiting..."),
    }

    match price_state.orca_raw() {
        Some(o) => println!("│  DEX (Orca):     ${:.6}  liq=${:.0}",
            o.price, o.liquidity_usd),
        None => println!("│  DEX (Orca):     waiting..."),
    }

    match price_state.raydium_raw() {
        Some(r) => println!("│  DEX (Raydium):  ${:.6}  liq=${:.0}",
            r.price, r.liquidity_usd),
        None => println!("│  DEX (Raydium):  waiting..."),
    }

    match price_state.snapshot() {
        Some(snap) => {
            let spread = snap.spread_pct();
            let arrow  = if spread > 0.0 { "▲" } else { "▼" };
            println!("│  Spread: {:+.4}% {}  │  Slot: {}",
                spread, arrow, snap.current_slot);
        }
        None => println!("│  Spread: --  (snapshot not ready)"),
    }
    println!("└{}", "─".repeat(w - 1));

    // ── Detector stats ────────────────────────────────────────────────────
    let det_stats = detector.stats();
    println!("┌─ Detector {}", "─".repeat(w - 12));
    println!("│  checks={} signals={} hit={:.3}%",
        det_stats.checks_total,
        det_stats.signals_emitted,
        det_stats.hit_rate_pct(),
    );
    println!("│  rej: spread={} liq={} stale={} cool={} profit={}",
        det_stats.rejected_spread,
        det_stats.rejected_liquidity,
        det_stats.rejected_staleness,
        det_stats.rejected_cooldown,
        det_stats.rejected_profit,
    );

    let disp = display.lock().await;
    println!("│  skip: open={} no_bal={} neg_fill={} no_snap={}",
        disp.skip_counts.trade_open,
        disp.skip_counts.no_balance,
        disp.skip_counts.neg_fills,
        disp.skip_counts.no_snapshot,
    );
    println!("└{}", "─".repeat(w - 1));

    // ── Recent signals ────────────────────────────────────────────────────
    println!("┌─ Recent Signals (last 5) {}", "─".repeat(w - 27));
    if disp.signal_log.is_empty() {
        println!("│  (none yet — waiting for spread > {:.2}%)",
            0.15);
    } else {
        for sig in disp.signal_log.iter().rev().take(5) {
            println!("│  [{slot}] {dir} {spread:+.4}% → net=${net:.4}",
                slot   = sig.dex_slot,
                dir    = sig.direction,
                spread = sig.spread_pct,
                net    = sig.net_profit_usdt,
            );
        }
    }
    println!("└{}", "─".repeat(w - 1));

    // ── Recent trades ─────────────────────────────────────────────────────
    println!("┌─ Recent Trades (last 5) {}", "─".repeat(w - 26));
    if disp.trade_log.is_empty() {
        println!("│  (none yet)");
    } else {
        for rec in disp.trade_log.iter().rev().take(5) {
            let emoji = match rec.outcome {
                pnl::paper::paper_engine::TradeOutcome::Win       => "✅",
                pnl::paper::paper_engine::TradeOutcome::Loss      => "❌",
                pnl::paper::paper_engine::TradeOutcome::Breakeven => "➖",
            };
            println!("│  {emoji} #{id} {out} | {dir} | net=${net:+.4} | bal=${bal:.2}",
                emoji = emoji,
                id    = rec.id,
                out   = rec.outcome,
                dir   = rec.direction,
                net   = rec.net_profit_usdt,
                bal   = rec.balance_after,
            );
        }
    }
    drop(disp);
    println!("└{}", "─".repeat(w - 1));

    // ── Session stats ─────────────────────────────────────────────────────
    println!("┌─ Session Stats {}", "─".repeat(w - 17));
    let snap = tracker.lock().await.snapshot();
    if snap.has_trades() {
        println!("│  Balance:  ${:.2}  ({:+.4}% return)",
            snap.current_balance_usdt, snap.return_pct);
        println!("│  Trades:   {}  W:{} L:{} BE:{}  WR:{:.1}%",
            snap.total_trades, snap.wins, snap.losses,
            snap.breakevens, snap.win_rate_pct);
        println!("│  PnL: net=${:.4}  gross=${:.4}  fees=${:.4}",
            snap.total_pnl_usdt, snap.gross_pnl_usdt, snap.total_fees_usdt);
        println!("│  Avg: win=${:.4}  loss=${:.4}  expect=${:.4}",
            snap.avg_win_usdt, snap.avg_loss_usdt, snap.expectancy_usdt);
        println!("│  PF: {:.2}x  Sharpe: {:.4}  MaxDD: ${:.2}",
            snap.profit_factor, snap.sharpe_raw, snap.max_drawdown_usdt);
        println!("│  Streak: W{}/L{}  MaxW:{} MaxL:{}  Rate:{:.1}/hr",
            snap.current_win_streak, snap.current_loss_streak,
            snap.max_consecutive_wins, snap.max_consecutive_losses,
            snap.trades_per_hour);
        println!("│  Elapsed: {}", snap.elapsed_human());
    } else {
        println!("│  No trades yet — spread must exceed {:.2}% net to fire",
            0.15);
        println!("│  Elapsed: {}", snap.elapsed_human());
    }
    println!("└{}", "─".repeat(w - 1));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fast pseudo-random f64 in [0, 1) — no external crate needed.
/// Uses a thread-local LCG seeded from system time.
fn fast_rand() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );
    }
    STATE.with(|s| {
        // Knuth multiplicative LCG
        let next = s.get()
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s.set(next);
        // Top 53 bits → f64 in [0, 1)
        (next >> 11) as f64 / (1u64 << 53) as f64
    })
}

#[inline]
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}