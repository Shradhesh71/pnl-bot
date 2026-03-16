//! Live trading binary — full pipeline with real Binance WS and optional
//! real execution.
//!
//! # Usage
//!
//! ```bash
//! # Paper mode, SOL only (default):
//! GEYSER_ENDPOINT="https://solana-rpc.parafi.tech:10443" \
//! GEYSER_X_TOKEN="your-token" \
//! cargo run --bin live_trade
//!
//! # All 4 pairs (SOL, WIF, BONK, JTO):
//! PAIRS="solusdc,wifusdc,bonkusdc,jtousdc" \
//! GEYSER_ENDPOINT="..." GEYSER_X_TOKEN="..." \
//! cargo run --bin live_trade
//!
//! # Live mode (real orders — prerequisites must be met first):
//! LIVE=true LIVE_CONFIRMED=yes \
//! BINANCE_API_KEY="..." BINANCE_API_SECRET="..." \
//! WALLET_KEYPAIR_PATH="/path/to/keypair.json" \
//! cargo run --bin live_trade
//! ```
//!
//! # Environment variables
//!
//! | Variable          | Default    | Description                           |
//! |-------------------|------------|---------------------------------------|
//! | GEYSER_ENDPOINT   | required   | Yellowstone gRPC URL                  |
//! | GEYSER_X_TOKEN    | required   | Geyser auth token                     |
//! | PAIRS             | solusdc    | Comma-separated Binance symbols       |
//! | INITIAL_BALANCE   | 10000.0    | Starting USDC for paper trading       |
//! | TRADE_SIZE        | 1000.0     | Notional per trade (USDC)             |
//! | LIVE              | false      | Enable live order execution           |
//! | LIVE_CONFIRMED    | —          | Must be "yes" when LIVE=true          |

use std::{collections::VecDeque, env, sync::Arc};

use tokio::{signal, sync::Mutex, time::{interval, Duration}};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use pnl::{
    dex::pool_monitor::{PoolMonitorConfig, start_pool_monitor},
    execution::{
        binance_rest::BinanceClient,
        binance_ws::{BinanceWsConfig, start_binance_ws},
        coordinator::{Coordinator, CoordinatorConfig, CoordinatorResult},
        jupiter::{JupiterClient, JupiterConfig},
    },
    paper::{
        paper_engine::{PaperEngine, PaperEngineConfig, TradeOutcome, TradeRecord},
        pnl_tracker::PnlTracker,
    },
    strategy::{
        price_state::{PriceState, PriceStateConfig},
        spread_detector::{SpreadConfig, SpreadDetector},
    },
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG").unwrap_or_else(|_| "warn,live_trade=info".into())
        )
        .with_target(false)
        .init();

    let live_mode = env::var("LIVE").map(|v| v == "true").unwrap_or(false);

    info!("=== live_trade starting ===");
    info!("Mode: {}", if live_mode { "LIVE (real orders)" } else { "PAPER (no real orders)" });

    if live_mode {
        let confirmed = env::var("LIVE_CONFIRMED").map(|v| v == "yes").unwrap_or(false);
        if !confirmed {
            eprintln!("\nLIVE mode requires LIVE_CONFIRMED=yes environment variable.");
            eprintln!("This will place REAL orders on Binance and Solana.");
            eprintln!("Set LIVE_CONFIRMED=yes only when you are ready.\n");
            std::process::exit(1);
        }
    }

    // ── Which pairs to watch ──────────────────────────────────────────────
    // Default: solusdc only.  Set PAIRS="solusdc,wifusdc,bonkusdc,jtousdc"
    // to watch all four simultaneously.
    let pairs: Vec<String> = env::var("PAIRS")
        .unwrap_or_else(|_| "solusdc".to_string())
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();

    info!("Watching pairs: {:?}", pairs);

    // ── Shared state ──────────────────────────────────────────────────────
    let price_state = Arc::new(PriceState::new(PriceStateConfig {
        max_cex_age_ms:   5_000,
        max_dex_slot_age: 15,
        ..PriceStateConfig::default()
    }));

    let initial_balance: f64 = env::var("INITIAL_BALANCE")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(10_000.0);
    let trade_size: f64 = env::var("TRADE_SIZE")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000.0);

    let engine = Arc::new(Mutex::new(PaperEngine::new(PaperEngineConfig {
        initial_balance_usdt: initial_balance,
        trade_size_usdt:      trade_size,
        ..PaperEngineConfig::default()
    })));
    let tracker   = Arc::new(Mutex::new(PnlTracker::new(initial_balance)));
    let trade_log = Arc::new(Mutex::new(VecDeque::<TradeRecord>::with_capacity(10)));

    let detector = Arc::new(SpreadDetector::new(SpreadConfig {
        trade_size_usdt: trade_size,
        ..SpreadConfig::default()
    }));

    // ── Execution clients ─────────────────────────────────────────────────
    let binance = Arc::new(if live_mode {
        BinanceClient::from_env()
    } else {
        BinanceClient::new_paper()
    });

    let jupiter = Arc::new(if live_mode {
        JupiterClient::from_env(JupiterConfig::from_env())
    } else {
        JupiterClient::new_paper()
    });

    let coordinator = Arc::new(Mutex::new(Coordinator::new(
        CoordinatorConfig {
            paper_mode:      !live_mode,
            trade_size_usdt: trade_size,
            ..CoordinatorConfig::default()
        },
        Arc::clone(&binance),
        Arc::clone(&jupiter),
        Arc::clone(&engine),
        Arc::clone(&tracker),
    )));

    let cancel = CancellationToken::new();

    // ── Pool monitor ──────────────────────────────────────────────────────
    let pool_cfg = PoolMonitorConfig::from_env()?;
    info!("Pool monitor: {} pools", pool_cfg.pools.len());
    let (_pool_handle, pool_rx) = start_pool_monitor(pool_cfg).await?;

    // ── Binance WS — single or combined multi-pair stream ────────────────
    // 1 symbol  → wss://stream.binance.com/ws/<symbol>@bookTicker
    // N symbols → wss://stream.binance.com/stream?streams=<s1>/<s2>/...
    let ws_config = if pairs.len() == 1 {
        BinanceWsConfig::default()  // solusdc single stream
    } else {
        BinanceWsConfig::multi(pairs.iter().map(|s| s.as_str()).collect())
    };

    info!("Binance WS symbols: {:?}", ws_config.symbols);
    let _ws_handle = start_binance_ws(
        ws_config,
        Arc::clone(&price_state),
        cancel.clone(),
    ).await?;

    // ── Launch tasks ──────────────────────────────────────────────────────
    let t1 = tokio::spawn(geyser_task(
        pool_rx,
        Arc::clone(&price_state),
        cancel.clone(),
    ));

    let t2 = tokio::spawn(detector_task(
        Arc::clone(&price_state),
        Arc::clone(&detector),
        Arc::clone(&coordinator),
        Arc::clone(&trade_log),
        cancel.clone(),
    ));

    let t3 = tokio::spawn(display_task(
        Arc::clone(&price_state),
        Arc::clone(&detector),
        Arc::clone(&tracker),
        Arc::clone(&trade_log),
        pairs.clone(),
        live_mode,
        cancel.clone(),
    ));

    // ── Ctrl+C ────────────────────────────────────────────────────────────
    signal::ctrl_c().await?;
    println!("\nShutting down...");
    cancel.cancel();

    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        let _ = tokio::join!(t1, t2, t3);
    }).await;

    println!("\n{}", "═".repeat(57));
    println!("  FINAL SESSION STATS");
    println!("{}", "═".repeat(57));
    println!("{}", tracker.lock().await.snapshot());
    println!("{}", "═".repeat(57));

    Ok(())
}

// ---------------------------------------------------------------------------
// Task 1: Geyser → PriceState
// ---------------------------------------------------------------------------

async fn geyser_task(
    mut rx:      tokio::sync::watch::Receiver<Option<pnl::dex::pool_monitor::PoolPrice>>,
    price_state: Arc<PriceState>,
    cancel:      CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = rx.changed() => {
                if result.is_err() { break; }
                if let Some(p) = rx.borrow().clone() {
                    price_state.update_dex(&p);
                    price_state.update_slot(p.slot);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Task 2: Detector → Coordinator (50ms tick)
// ---------------------------------------------------------------------------

async fn detector_task(
    price_state: Arc<PriceState>,
    detector:    Arc<SpreadDetector>,
    coordinator: Arc<Mutex<Coordinator>>,
    trade_log:   Arc<Mutex<VecDeque<TradeRecord>>>,
    cancel:      CancellationToken,
) {
    let mut tick = interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                // Need fresh snapshot to pass to coordinator
                let snap = match price_state.snapshot() {
                    Some(s) => s,
                    None    => continue,
                };

                let signal = match detector.check(&snap) {
                    Some(s) => s,
                    None    => continue,
                };

                // Pass snapshot so coordinator can check slot freshness
                let result = coordinator.lock().await
                    .on_signal(signal).await;

                match result {
                    CoordinatorResult::Filled(rec) => {
                        info!(
                            "LIVE TRADE: net=${:.4} latency={}ms sig={}",
                            rec.net_profit_usdt, rec.total_latency_ms, rec.dex_signature
                        );
                    }

                    CoordinatorResult::PaperModeSkipped => {
                        // Coordinator already called PaperEngine + PnlTracker.
                        // Pull the most recent trade out of the engine and push
                        // it to trade_log so the display can show it.
                        // We check engine's last_trade() if that method exists,
                        // otherwise the coordinator logs it via tracing::info.
                        // For now the display reads from tracker stats directly.
                    }

                    CoordinatorResult::PrerequisitesNotMet { reason } => {
                        info!("Prerequisites not met: {reason}");
                    }
                    CoordinatorResult::SignalStale { slot_age } => {
                        warn!("Signal stale: slot_age={slot_age}");
                    }
                    CoordinatorResult::LegFailed { cex_err, dex_err } => {
                        if let Some(e) = cex_err { tracing::error!("CEX leg: {e}"); }
                        if let Some(e) = dex_err { tracing::error!("DEX leg: {e}"); }
                    }
                    CoordinatorResult::InsufficientBalance { usdc, required } => {
                        warn!("Insufficient balance: ${usdc:.2} < ${required:.2}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Task 3: Display (1s redraw)
// ---------------------------------------------------------------------------

async fn display_task(
    price_state: Arc<PriceState>,
    detector:    Arc<SpreadDetector>,
    tracker:     Arc<Mutex<PnlTracker>>,
    trade_log:   Arc<Mutex<VecDeque<TradeRecord>>>,
    pairs:       Vec<String>,
    live_mode:   bool,
    cancel:      CancellationToken,
) {
    let mut tick  = interval(Duration::from_secs(1));
    let mut frame = 0u64;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tick.tick() => {
                frame += 1;
                render(
                    frame, &price_state, &detector,
                    &tracker, &trade_log, &pairs, live_mode,
                ).await;
            }
        }
    }
}

async fn render(
    frame:       u64,
    price_state: &Arc<PriceState>,
    detector:    &Arc<SpreadDetector>,
    tracker:     &Arc<Mutex<PnlTracker>>,
    trade_log:   &Arc<Mutex<VecDeque<TradeRecord>>>,
    pairs:       &[String],
    live_mode:   bool,
) {
    print!("\x1b[2J\x1b[H");
    let w        = 57usize;
    let mode_str = if live_mode { "LIVE" } else { "PAPER" };

    println!("{}", "═".repeat(w));
    println!("  [{mode_str}]  frame={frame}  |  Ctrl+C to stop");
    println!("{}", "═".repeat(w));

    // ── Live Prices ───────────────────────────────────────────────────────
    println!("┌─ Live Prices {}", "─".repeat(w - 15));

    // SOL/USDC always shown from primary AtomicCell
    match price_state.cex_raw() {
        Some(c) => {
            let age_ms = (now_us() - c.updated_at_us) / 1000;
            println!("│  SOLUSDC  CEX ${:.4}  bid={:.4} ask={:.4}  [{age_ms}ms ago]",
                c.price, c.bid, c.ask);
        }
        None => println!("│  SOLUSDC  CEX connecting..."),
    }

    // Additional pairs from per-pair map (WIF, BONK, JTO)
    for pair in pairs.iter().filter(|p| p.to_uppercase() != "SOLUSDC") {
        let sym = pair.to_uppercase();
        match price_state.cex_for_pair_raw(&sym) {
            Some(c) => {
                let age_ms = (now_us() - c.updated_at_us) / 1000;
                println!("│  {sym:<8} CEX ${:.6}  bid={:.6} ask={:.6}  [{age_ms}ms ago]",
                    c.price, c.bid, c.ask);
            }
            None => println!("│  {sym:<8} CEX connecting..."),
        }
    }

    // DEX prices (always SOL/USDC pools for now — expand when pool_monitor
    // supports additional pairs)
    match price_state.orca_raw() {
        Some(o) => {
            let age = price_state.current_slot().saturating_sub(o.slot);
            println!("│  SOLUSDC  Orca    ${:.4}  liq=${:.0}  [{age}slot ~{}ms]",
                o.price, o.liquidity_usd, age * 400);
        }
        None => println!("│  SOLUSDC  Orca    waiting..."),
    }
    match price_state.raydium_raw() {
        Some(r) => {
            let age = price_state.current_slot().saturating_sub(r.slot);
            println!("│  SOLUSDC  Raydium ${:.4}  liq=${:.0}  [{age}slot ~{}ms]",
                r.price, r.liquidity_usd, age * 400);
        }
        None => println!("│  SOLUSDC  Raydium waiting..."),
    }

    match price_state.snapshot() {
        Some(snap) => println!("│  Spread {:+.4}%  |  Slot {}",
            snap.spread_pct(), snap.current_slot),
        None => println!("│  Spread --  (waiting for CEX and DEX)"),
    }
    println!("└{}", "─".repeat(w - 1));

    // ── Detector ──────────────────────────────────────────────────────────
    let ds = detector.stats();
    println!("┌─ Detector {}", "─".repeat(w - 12));
    println!("│  checks={} signals={} hit={:.3}%",
        ds.checks_total, ds.signals_emitted, ds.hit_rate_pct());
    println!("│  rej: spread={} liq={} stale={} cool={} profit={}",
        ds.rejected_spread, ds.rejected_liquidity,
        ds.rejected_staleness, ds.rejected_cooldown, ds.rejected_profit);
    println!("└{}", "─".repeat(w - 1));

    // ── Recent Trades ─────────────────────────────────────────────────────
    println!("┌─ Recent Trades (last 5) {}", "─".repeat(w - 26));
    let log = trade_log.lock().await;
    if log.is_empty() {
        println!("│  (none yet)");
    } else {
        for rec in log.iter().rev().take(5) {
            let tag = match rec.outcome {
                TradeOutcome::Win       => "WIN ",
                TradeOutcome::Loss      => "LOSS",
                TradeOutcome::Breakeven => "EVEN",
            };
            println!("│  [{tag}] #{} {} net=${:+.4} bal=${:.2}",
                rec.id, rec.direction, rec.net_profit_usdt, rec.balance_after);
        }
    }
    drop(log);
    println!("└{}", "─".repeat(w - 1));

    // ── Session Stats ─────────────────────────────────────────────────────
    println!("┌─ Session Stats {}", "─".repeat(w - 17));
    let snap = tracker.lock().await.snapshot();
    if snap.has_trades() {
        println!("│  Balance ${:.2}  ({:+.4}% return)",
            snap.current_balance_usdt, snap.return_pct);
        println!("│  Trades {}  W:{} L:{}  WR:{:.1}%  PF:{:.2}x",
            snap.total_trades, snap.wins, snap.losses,
            snap.win_rate_pct, snap.profit_factor);
        println!("│  PnL ${:.4}  Sharpe {:.4}  MaxDD ${:.2}",
            snap.total_pnl_usdt, snap.sharpe_raw, snap.max_drawdown_usdt);
        println!("│  Streak W{}/L{}  Rate {:.1}/hr  {}",
            snap.current_win_streak, snap.current_loss_streak,
            snap.trades_per_hour, snap.elapsed_human());
    } else {
        println!("│  No trades yet");
        println!("│  Elapsed: {}", snap.elapsed_human());
    }
    println!("└{}", "─".repeat(w - 1));

    // ── Live mode gate status ──────────────────────────────────────────────
    if !live_mode {
        let snap   = tracker.lock().await.snapshot();
        let needed = 50u64.saturating_sub(snap.total_trades);
        if needed > 0 {
            println!("  Need {needed} more paper trades before live mode unlocks");
        } else if snap.win_rate_pct < 60.0 {
            println!("  Win rate {:.1}% < 60% required for live mode", snap.win_rate_pct);
        } else {
            println!("  Prerequisites met -- set LIVE=true LIVE_CONFIRMED=yes to go live");
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


// //! Live trading binary — full pipeline with real Binance WS and optional
// //! real execution.
// //!
// //! # Usage
// //!
// //! ```bash
// //! # Paper mode (default — no real orders):
// //! GEYSER_ENDPOINT="https://solana-rpc.parafi.tech:10443" \
// //! GEYSER_X_TOKEN="your-token" \
// //! cargo run --bin live_trade
// //!
// //! # Live mode (real orders — only after 50 profitable paper trades):
// //! GEYSER_ENDPOINT="..." \
// //! GEYSER_X_TOKEN="..." \
// //! BINANCE_API_KEY="..." \
// //! BINANCE_API_SECRET="..." \
// //! WALLET_KEYPAIR_PATH="/path/to/keypair.json" \
// //! LIVE=true \
// //! cargo run --bin live_trade
// //! ```
// //!
// //! # Difference from paper_trade_demo
// //!
// //! | Feature              | paper_trade_demo      | live_trade              |
// //! |----------------------|-----------------------|-------------------------|
// //! | CEX price            | Fake random walk      | ✅ Real Binance WS      |
// //! | DEX price            | ✅ Real Geyser        | ✅ Real Geyser          |
// //! | CEX execution        | Simulated             | ✅ Real Binance REST    |
// //! | DEX execution        | Simulated             | ✅ Real Jupiter swap    |
// //! | Safety gates         | Minimal               | ✅ Full (50 trades req) |

// use std::{collections::VecDeque, env, sync::Arc};

// use tokio::{signal, sync::Mutex, time::{interval, Duration}};
// use tokio_util::sync::CancellationToken;
// use tracing::{info, warn};

// use pnl::{
//     dex::pool_monitor::{PoolMonitorConfig, start_pool_monitor},
//     execution::{
//         binance_rest::BinanceClient,
//         binance_ws::{BinanceWsConfig, start_binance_ws},
//         coordinator::{Coordinator, CoordinatorConfig, CoordinatorResult},
//         jupiter::{JupiterClient, JupiterConfig},
//     },
//     paper::{
//         paper_engine::{PaperEngine, PaperEngineConfig, TradeRecord},
//         pnl_tracker::PnlTracker,
//     },
//     strategy::{
//         price_state::{PriceState, PriceStateConfig},
//         spread_detector::{SpreadConfig, SpreadDetector},
//     },
// };

// // ---------------------------------------------------------------------------
// // Entry point
// // ---------------------------------------------------------------------------

// #[tokio::main]
// async fn main() -> anyhow::Result<()> {
//     tracing_subscriber::fmt()
//         .with_env_filter(
//             env::var("RUST_LOG").unwrap_or_else(|_| "warn,live_trade=info".into())
//         )
//         .with_target(false)
//         .init();

//     let live_mode = env::var("LIVE").map(|v| v == "true").unwrap_or(false);

//     info!("=== live_trade starting ===");
//     info!("Mode: {}", if live_mode { "🔴 LIVE (real orders)" } else { "🟡 PAPER (no real orders)" });

//     if live_mode {
//         // Require explicit confirmation
//         let confirmed = env::var("LIVE_CONFIRMED").map(|v| v == "yes").unwrap_or(false);
//         if !confirmed {
//             eprintln!("\n⚠️  LIVE mode requires LIVE_CONFIRMED=yes environment variable.");
//             eprintln!("    This will place REAL orders on Binance and Solana.");
//             eprintln!("    Set LIVE_CONFIRMED=yes only when you are ready.\n");
//             std::process::exit(1);
//         }
//     }

//     // ── Shared state ──────────────────────────────────────────────────────
//     let price_state = Arc::new(PriceState::new(PriceStateConfig::default()));

//     let initial_balance = env::var("INITIAL_BALANCE")
//         .ok().and_then(|v| v.parse().ok()).unwrap_or(10_000.0_f64);
//     let trade_size = env::var("TRADE_SIZE")
//         .ok().and_then(|v| v.parse().ok()).unwrap_or(1_000.0_f64);

//     let engine  = Arc::new(Mutex::new(PaperEngine::new(PaperEngineConfig {
//         initial_balance_usdt: initial_balance,
//         trade_size_usdt:      trade_size,
//         ..PaperEngineConfig::default()
//     })));
//     let tracker = Arc::new(Mutex::new(PnlTracker::new(initial_balance)));
//     let trade_log: Arc<Mutex<VecDeque<TradeRecord>>> =
//         Arc::new(Mutex::new(VecDeque::with_capacity(10)));

//     let detector = Arc::new(SpreadDetector::new(SpreadConfig {
//         trade_size_usdt: trade_size,
//         ..SpreadConfig::default()
//     }));

//     // ── Execution clients ─────────────────────────────────────────────────
//     let binance = Arc::new(if live_mode {
//         BinanceClient::from_env()
//     } else {
//         BinanceClient::new_paper()
//     });

//     let jupiter = Arc::new(if live_mode {
//         JupiterClient::from_env(JupiterConfig::from_env())
//     } else {
//         JupiterClient::new_paper()
//     });

//     let coordinator = Arc::new(Mutex::new(Coordinator::new(
//         CoordinatorConfig {
//             paper_mode:   !live_mode,
//             trade_size_usdt: trade_size,
//             ..CoordinatorConfig::default()
//         },
//         Arc::clone(&binance),
//         Arc::clone(&jupiter),
//         Arc::clone(&engine),
//         Arc::clone(&tracker),
//     )));

//     let cancel = CancellationToken::new();

//     // ── Pool monitor ──────────────────────────────────────────────────────
//     let pool_cfg = PoolMonitorConfig::from_env()?;
//     info!("Pool monitor: {} pools", pool_cfg.pools.len());
//     let (_pool_handle, pool_rx) = start_pool_monitor(pool_cfg).await?;

//     // ── Real Binance WS ───────────────────────────────────────────────────
//     let _ws_handle = start_binance_ws(
//         BinanceWsConfig::default(),
//         Arc::clone(&price_state),
//         cancel.clone(),
//     ).await?;
//     info!("Binance WS: started (real feed)");

//     // ── Tasks ─────────────────────────────────────────────────────────────
//     let t1 = tokio::spawn(geyser_task(pool_rx, Arc::clone(&price_state), cancel.clone()));
//     let t2 = tokio::spawn(detector_task(
//         Arc::clone(&price_state),
//         Arc::clone(&detector),
//         Arc::clone(&coordinator),
//         // Arc::clone(&trade_log),
//         cancel.clone(),
//     ));
//     let t3 = tokio::spawn(display_task(
//         Arc::clone(&price_state),
//         Arc::clone(&detector),
//         Arc::clone(&tracker),
//         Arc::clone(&trade_log),
//         live_mode,
//         cancel.clone(),
//     ));

//     // ── Ctrl+C ────────────────────────────────────────────────────────────
//     signal::ctrl_c().await?;
//     println!("\nShutting down...");
//     cancel.cancel();

//     let _ = tokio::time::timeout(Duration::from_secs(3), async {
//         let _ = tokio::join!(t1, t2, t3);
//     }).await;

//     // ── Final stats ───────────────────────────────────────────────────────
//     println!("\n{}", "═".repeat(55));
//     println!("  FINAL SESSION STATS");
//     println!("{}", "═".repeat(55));
//     println!("{}", tracker.lock().await.snapshot());
//     println!("{}", "═".repeat(55));

//     Ok(())
// }

// // ---------------------------------------------------------------------------
// // Task 1: Geyser → PriceState
// // ---------------------------------------------------------------------------

// async fn geyser_task(
//     mut rx:      tokio::sync::watch::Receiver<Option<pnl::dex::pool_monitor::PoolPrice>>,
//     price_state: Arc<PriceState>,
//     cancel:      CancellationToken,
// ) {
//     loop {
//         tokio::select! {
//             _ = cancel.cancelled() => break,
//             result = rx.changed() => {
//                 if result.is_err() { break; }
//                 if let Some(p) = rx.borrow().clone() {
//                     price_state.update_dex(&p);
//                     price_state.update_slot(p.slot);
//                 }
//             }
//         }
//     }
// }

// // ---------------------------------------------------------------------------
// // Task 2: Detector → Coordinator (50ms tick)
// // ---------------------------------------------------------------------------

// async fn detector_task(
//     price_state:  Arc<PriceState>,
//     detector:     Arc<SpreadDetector>,
//     coordinator:  Arc<Mutex<Coordinator>>,
//     // trade_log:    Arc<Mutex<VecDeque<TradeRecord>>>,
//     cancel:       CancellationToken,
// ) {
//     let mut tick = interval(Duration::from_millis(50));

//     loop {
//         tokio::select! {
//             _ = cancel.cancelled() => break,
//             _ = tick.tick() => {
//                 let snap = match price_state.snapshot() {
//                     Some(s) => s,
//                     None    => continue,
//                 };

//                 let signal = match detector.check(&snap) {
//                     Some(s) => s,
//                     None    => continue,
//                 };

//                 let result = coordinator.lock().await
//                     .on_signal(signal).await;

//                 match result {
//                     CoordinatorResult::Filled(rec) => {
//                         info!("LIVE TRADE: net=${:.4} latency={}ms sig={}",
//                             rec.net_profit_usdt, rec.total_latency_ms, rec.dex_signature);
//                     }
//                     CoordinatorResult::PaperModeSkipped => {
//                         // Paper trades logged inside coordinator
//                     }
//                     CoordinatorResult::PrerequisitesNotMet { reason } => {
//                         // Only log once per minute to avoid spam
//                         info!("Prerequisites: {reason}");
//                     }
//                     CoordinatorResult::SignalStale { slot_age } => {
//                         warn!("Signal stale slot_age={slot_age}");
//                     }
//                     CoordinatorResult::LegFailed { cex_err, dex_err } => {
//                         if let Some(e) = cex_err { tracing::error!("CEX leg: {e}"); }
//                         if let Some(e) = dex_err { tracing::error!("DEX leg: {e}"); }
//                     }
//                     CoordinatorResult::InsufficientBalance { usdc, required } => {
//                         warn!("Insufficient balance: ${usdc:.2} < ${required:.2}");
//                     }
//                 }
//             }
//         }
//     }
// }

// // ---------------------------------------------------------------------------
// // Task 3: Display (1s tick) — same layout as paper_trade_demo
// // ---------------------------------------------------------------------------

// async fn display_task(
//     price_state: Arc<PriceState>,
//     detector:    Arc<SpreadDetector>,
//     tracker:     Arc<Mutex<PnlTracker>>,
//     trade_log:   Arc<Mutex<VecDeque<TradeRecord>>>,
//     live_mode:   bool,
//     cancel:      CancellationToken,
// ) {
//     let mut tick  = interval(Duration::from_secs(1));
//     let mut frame = 0u64;

//     loop {
//         tokio::select! {
//             _ = cancel.cancelled() => break,
//             _ = tick.tick() => {
//                 frame += 1;
//                 render(frame, &price_state, &detector, &tracker, &trade_log, live_mode).await;
//             }
//         }
//     }
// }

// async fn render(
//     frame:       u64,
//     price_state: &Arc<PriceState>,
//     detector:    &Arc<SpreadDetector>,
//     tracker:     &Arc<Mutex<PnlTracker>>,
//     trade_log:   &Arc<Mutex<VecDeque<TradeRecord>>>,
//     live_mode:   bool,
// ) {
//     print!("\x1b[2J\x1b[H");
//     let w = 57usize;
//     let mode_str = if live_mode { "🔴 LIVE" } else { "🟡 PAPER" };

//     println!("{}", "═".repeat(w));
//     println!("  {}  │  frame={}  │  Ctrl+C to stop", mode_str, frame);
//     println!("{}", "═".repeat(w));

//     // Live prices
//     println!("┌─ Live Prices (REAL Binance WS) {}", "─".repeat(w - 33));
//     match price_state.cex_raw() {
//         Some(c) => println!("│  CEX (Binance):  ${:.6}  bid=${:.6} ask=${:.6}",
//             c.price, c.bid, c.ask),
//         None    => println!("│  CEX (Binance):  connecting..."),
//     }
//     match price_state.orca_raw() {
//         Some(o) => println!("│  DEX (Orca):     ${:.6}  liq=${:.0}", o.price, o.liquidity_usd),
//         None    => println!("│  DEX (Orca):     waiting..."),
//     }
//     match price_state.raydium_raw() {
//         Some(r) => println!("│  DEX (Raydium):  ${:.6}  liq=${:.0}", r.price, r.liquidity_usd),
//         None    => println!("│  DEX (Raydium):  waiting..."),
//     }
//     match price_state.snapshot() {
//         Some(snap) => println!("│  Spread: {:+.4}%  │  Slot: {}", snap.spread_pct(), snap.current_slot),
//         None       => println!("│  Spread: --"),
//     }
//     println!("└{}", "─".repeat(w - 1));

//     // Detector
//     let ds = detector.stats();
//     println!("┌─ Detector {}", "─".repeat(w - 12));
//     println!("│  checks={} signals={} hit={:.3}%",
//         ds.checks_total, ds.signals_emitted, ds.hit_rate_pct());
//     println!("│  rej: spread={} liq={} stale={} cool={} profit={}",
//         ds.rejected_spread, ds.rejected_liquidity,
//         ds.rejected_staleness, ds.rejected_cooldown, ds.rejected_profit);
//     println!("└{}", "─".repeat(w - 1));

//     // Recent trades
//     println!("┌─ Recent Trades (last 5) {}", "─".repeat(w - 26));
//     let log = trade_log.lock().await;
//     if log.is_empty() {
//         println!("│  (none yet)");
//     } else {
//         for rec in log.iter().rev().take(5) {
//             println!("│  {} #{} {} net=${:+.4} bal=${:.2}",
//                 match rec.outcome {
//                     pnl::paper::paper_engine::TradeOutcome::Win       => "✅",
//                     pnl::paper::paper_engine::TradeOutcome::Loss      => "❌",
//                     pnl::paper::paper_engine::TradeOutcome::Breakeven => "➖",
//                 },
//                 rec.id, rec.direction, rec.net_profit_usdt, rec.balance_after);
//         }
//     }
//     drop(log);
//     println!("└{}", "─".repeat(w - 1));

//     // Session stats
//     println!("┌─ Session Stats {}", "─".repeat(w - 17));
//     let snap = tracker.lock().await.snapshot();
//     if snap.has_trades() {
//         println!("│  Balance: ${:.2}  ({:+.4}% return)",
//             snap.current_balance_usdt, snap.return_pct);
//         println!("│  Trades: {}  W:{}  L:{}  WR:{:.1}%  PF:{:.2}x",
//             snap.total_trades, snap.wins, snap.losses,
//             snap.win_rate_pct, snap.profit_factor);
//         println!("│  PnL: ${:.4}  Sharpe:{:.4}  MaxDD:${:.2}",
//             snap.total_pnl_usdt, snap.sharpe_raw, snap.max_drawdown_usdt);
//         println!("│  Elapsed: {}", snap.elapsed_human());
//     } else {
//         println!("│  No trades yet");
//         println!("│  Elapsed: {}", snap.elapsed_human());
//     }
//     println!("└{}", "─".repeat(w - 1));

//     // Live mode gate status
//     if !live_mode {
//         let snap = tracker.lock().await.snapshot();
//         let trades_needed = 20u64.saturating_sub(snap.total_trades);
//         if trades_needed > 0 {
//             println!("  ⏳ Need {} more paper trades before live mode unlocks", trades_needed);
//         } else if snap.win_rate_pct < 60.0 {
//             println!("  ⚠️  Win rate {:.1}% < 60% required for live mode", snap.win_rate_pct);
//         } else {
//             println!("  ✅ Paper prerequisites met — set LIVE=true LIVE_CONFIRMED=yes to go live");
//         }
//     }
// }