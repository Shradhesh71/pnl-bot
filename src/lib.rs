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
pub mod strategy;
pub mod paper;

pub use dex::whirlpool::{
    DecimalConfig,
    WhirlpoolPrice,
    WhirlpoolState,
    WhirlpoolDecodeError,
    decode_whirlpool_price,
};

pub use dex::raydium::{
    RaydiumPrice,
    RaydiumDecodeError,
    decode_raydium_price,
};

pub use dex::pool_monitor::{
    PoolPrice,
    DexKind,
    PoolMonitorConfig,
    start_pool_monitor,
};

/// Top-level error type — wraps errors from all subsystems.
#[derive(Debug)]
pub enum ArbError {
    Dex(String),
    Strategy(String),
    Execution(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ArbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dex(s)       => write!(f, "dex error: {s}"),
            Self::Strategy(s)  => write!(f, "strategy error: {s}"),
            Self::Execution(s) => write!(f, "execution error: {s}"),
            Self::Io(e)        => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for ArbError {}

impl From<std::io::Error> for ArbError {
    fn from(e: std::io::Error) -> Self { Self::Io(e) }
}