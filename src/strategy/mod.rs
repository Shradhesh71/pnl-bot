//! Strategy layer — price aggregation and spread detection.
//!
//! # Submodules
//!
//! | Module             | Responsibility                                     |
//! |--------------------|----------------------------------------------------|
//! | `price_state`      | Thread-safe shared CEX + DEX price storage         |
//! | `spread_detector`  | Compute spread, emit `SpreadSignal` when profitable|
//!
//! # Data flow
//!
//! ```text
//! Binance WS  ──→  PriceState::update_cex()  ──┐
//!                                               ├──→  SpreadDetector::check()  ──→  SpreadSignal
//! Geyser      ──→  PriceState::update_dex()  ──┘
//! ```

pub mod price_state;
pub mod spread_detector;

// ── re-exports ──────────────────────────────────────────────────────────────

pub use price_state::{CexPrice, PriceState};

// pub use spread_detector::{
//     ArbDirection,
//     SpreadConfig,
//     SpreadDetector,
//     SpreadSignal,
// };