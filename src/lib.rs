//! DEX/CEX Arbitrage Engine
//!
//! # Module layout
//!
//! 
//! crate
//! ├── dex         — pool decoders + Geyser monitor
//! ├── strategy    — spread detection + shared price state
//! ├── paper       — paper trade engine + P&L tracker
//! └── execution   — live order execution (Phase 5)

pub mod dex;