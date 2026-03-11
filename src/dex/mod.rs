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
pub mod raydium;
pub mod pool_monitor;

// ── re-exports (so `use crate::dex::PoolPrice` works) ──────────────────────

pub use pool_monitor::{
    DexKind,
    PoolMonitorConfig,
    PoolPrice,
    start_pool_monitor,
};

pub use whirlpool::{
    DecimalConfig,
    WhirlpoolDecodeError,
    WhirlpoolPrice,
    WhirlpoolState,
    decode_whirlpool_price,
    decode_whirlpool_state,
};

pub use raydium::{
    RaydiumDecodeError,
    RaydiumPrice,
    decode_raydium_price,
};

// ── shared DEX types used by multiple submodules ────────────────────────────

/// A normalised DEX price update — the single type that `pool_monitor`
/// emits and `strategy::price_state` consumes.
/// Re-exported from `pool_monitor` but declared here so submodules can
/// import it without a circular dependency.
#[derive(Debug, Clone)]
pub struct NormalisedPoolPrice {
    pub dex: DexKind,
    pub symbol: String,      // e.g. "SOLUSDC"
    pub price: f64,          // human price (quote per base)
    pub fee_rate: f64,       // e.g. 0.0003
    pub liquidity_usd: f64,  // rough USD depth estimate
    pub slot: u64,
    pub received_at_us: i64,
}

impl From<WhirlpoolPrice> for NormalisedPoolPrice {
    fn from(p: WhirlpoolPrice) -> Self {
        Self {
            dex: DexKind::Orca,
            symbol: symbol_from_pool_address(&p.pool_address),
            price: p.price,
            fee_rate: p.fee_rate,
            liquidity_usd: p.liquidity_usd_approx,
            slot: p.slot,
            received_at_us: p.received_at_us,
        }
    }
}

impl From<RaydiumPrice> for NormalisedPoolPrice {
    fn from(p: RaydiumPrice) -> Self {
        Self {
            dex: DexKind::Raydium,
            symbol: p.symbol,
            price: p.price,
            fee_rate: p.fee_rate,
            liquidity_usd: p.liquidity_usd_approx,
            slot: p.slot,
            received_at_us: p.received_at_us,
        }
    }
}

/// Map known pool addresses to a canonical symbol string.
/// Falls back to the raw address for unknown pools.
pub fn symbol_from_pool_address(address: &str) -> String {
    match address {
        whirlpool::pools::SOL_USDC_64 | whirlpool::pools::SOL_USDC_8 => "SOLUSDC",
        whirlpool::pools::SOL_USDT_64 => "SOLUSDT",
        whirlpool::pools::ETH_USDC_64 => "ETHUSDC",
        whirlpool::pools::BTC_USDC_64 => "BTCUSDC",
        other => return other.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_from_known_pool() {
        assert_eq!(
            symbol_from_pool_address(whirlpool::pools::SOL_USDC_64),
            "SOLUSDC"
        );
    }

    #[test]
    fn test_symbol_from_unknown_pool() {
        let addr = "unknownpool111111111111111111111111";
        assert_eq!(symbol_from_pool_address(addr), addr);
    }
}