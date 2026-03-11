//! Paper trading layer — simulate trades and track P&L without real funds.
//!
//! # Submodules
//!
//! | Module           | Responsibility                                       |
//! |------------------|------------------------------------------------------|
//! | `paper_engine`   | Receives `SpreadSignal`, simulates fills, emits `TradeRecord` |
//! | `pnl_tracker`    | Aggregates `TradeRecord`s into rolling P&L stats     |
//! | `slippage_model` | Estimates realistic fill price from pool + book depth|
//!
//! # Data flow
//!
//! ```text
//! SpreadSignal
//!     │
//!     ▼
//! SlippageModel::adjust()   ← adjusts gross profit for realistic fill
//!     │
//!     ▼
//! PaperEngine::on_signal()  ← decides to trade, records result
//!     │
//!     ▼
//! TradeRecord
//!     │
//!     ▼
//! PnlTracker::record()      ← updates win_rate, drawdown, sharpe, etc.
//! ```

pub mod paper_engine;
pub mod pnl_tracker;
pub mod slippage_model;

// ── re-exports ──────────────────────────────────────────────────────────────

pub use paper_engine::{
    PaperEngine,
    PaperEngineConfig,
    TradeOutcome,
    TradeRecord,
};

pub use pnl_tracker::{
    PnlSnapshot,
    PnlTracker,
};

pub use slippage_model::{
    SlippageConfig,
    estimate_slippage,
};