//! DEX pool decoders and live price monitor.
//!
//! # Submodules
//!
//! | Module          | Responsibility                                      |
//! |-----------------|-----------------------------------------------------|
//! | `whirlpool`     | Decode Orca Whirlpool account bytes → price         |
//! | `raydium`       | Decode Raydium CLMM account bytes → price           |
//! | `pool_monitor`  | Subscribe to Geyser, emit unified `PoolPrice`       |

pub mod whirlpool;
pub mod pool_monitor;
pub mod raydium;