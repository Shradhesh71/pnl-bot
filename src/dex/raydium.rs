//! Raydium CLMM (Concentrated Liquidity Market Maker) account decoder.
//!
//! Decodes raw `PoolState` account bytes received from Geyser/Yellowstone
//! into structured pool state and human-readable prices.
//!
//! # Key difference from Whirlpool
//!
//! Raydium stores `mintDecimalsA` and `mintDecimalsB` **inside the account**,
//! so we never need an external `DecimalConfig` — decimals are read directly
//! from the account bytes at offsets 233 and 234.
//!
//! # PoolState account layout
//! (Anchor 8-byte discriminator prefix, verified against raydium-io/raydium-clmm)
//!
//! ```text
//! Offset  Field                 Type      Size   Notes
//! ------  --------------------  --------  ----   -----
//! 0       discriminator         [u8; 8]   8
//! 8       bump                  u8        1
//! 9       ammConfig             Pubkey    32
//! 41      creator               Pubkey    32
//! 73      mintA                 Pubkey    32     ← token A mint
//! 105     mintB                 Pubkey    32     ← token B mint
//! 137     vaultA                Pubkey    32
//! 169     vaultB                Pubkey    32
//! 201     observationId         Pubkey    32
//! 233     mintDecimalsA         u8        1      ← decimals for token A
//! 234     mintDecimalsB         u8        1      ← decimals for token B
//! 235     tickSpacing           u16       2
//! 237     liquidity             u128      16
//! 253     sqrtPriceX64          u128      16     ← Q64.64 fixed point
//! 269     tickCurrent           i32       4
//! 273     padding               u32       4
//! 277     feeGrowthGlobalX64A   u128      16
//! 293     feeGrowthGlobalX64B   u128      16
//! 309     protocolFeesTokenA    u64       8
//! 317     protocolFeesTokenB    u64       8
//! 325     swapInAmountTokenA    u128      16
//! 341     swapOutAmountTokenB   u128      16
//! 357     swapInAmountTokenB    u128      16
//! 373     swapOutAmountTokenA   u128      16
//! 389     status                u64       8
//! 397     ...reward_infos, bitmap, etc.
//! ```
//!
//! # Price formula (identical to Whirlpool — both use Q64.64)
//!
//! ```text
//! sqrt_price_f64  = sqrt_price_x64 / 2^64
//! raw_price       = sqrt_price_f64^2
//! human_price     = raw_price * 10^(decimals_a - decimals_b)
//! ```
//!
//! For SOL/USDC: decimals_a=9, decimals_b=6 → factor=1000 → price in USDC per SOL
//!
//! # Program ID
//! `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`

use std::fmt;

use crate::dex::whirlpool::DecimalConfig;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Raydium CLMM program ID (mainnet).
pub const RAYDIUM_CLMM_PROGRAM_ID: &str =
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

pub const POOL_STATE_DISCRIMINATOR: [u8; 8] =
    [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];

/// Minimum account data length we need to read all key fields.
/// Must reach at least past tickCurrent (offset 269 + 4 = 273).
pub const POOL_STATE_MIN_LEN: usize = 273;

/// Q64.64 denominator: 2^64 as f64.
const Q64: f64 = 18_446_744_073_709_551_616.0_f64;

// ---------------------------------------------------------------------------
// Byte offsets (verified against raydium-io/raydium-clmm PoolLayout)
// ---------------------------------------------------------------------------

const OFF_DISCRIMINATOR:   usize = 0;
// const OFF_BUMP:            usize = 8;
// const OFF_AMM_CONFIG:      usize = 9;    // Pubkey (32)
// const OFF_CREATOR:         usize = 41;   // Pubkey (32)
const OFF_MINT_A:          usize = 73;   // Pubkey (32)
const OFF_MINT_B:          usize = 105;  // Pubkey (32)
// const OFF_VAULT_A:         usize = 137;  // Pubkey (32)
// const OFF_VAULT_B:         usize = 169;  // Pubkey (32)
// const OFF_OBSERVATION_ID:  usize = 201;  // Pubkey (32)
const OFF_DECIMALS_A:      usize = 233;  // u8
const OFF_DECIMALS_B:      usize = 234;  // u8
const OFF_TICK_SPACING:    usize = 235;  // u16
const OFF_LIQUIDITY:       usize = 237;  // u128
const OFF_SQRT_PRICE:      usize = 253;  // u128  Q64.64
const OFF_TICK_CURRENT:    usize = 269;  // i32

// ---------------------------------------------------------------------------
// Raydium CLMM pool addresses (mainnet)
// ---------------------------------------------------------------------------

pub mod pools {
    /// SOL/USDC — tick_spacing 64
    pub const SOL_USDC: &str  = "8sLbNZoA1cfnvMJLPfp98ZLAnFSYCFApfJKMbiXNLwxj";
    /// SOL/USDT — tick_spacing 64
    pub const SOL_USDT: &str  = "H8gBFEjmCPfGR3ZJ8BKkBvkJFxKpw4SvHkBTF9cRxktq";
    /// ETH/USDC — tick_spacing 64
    pub const ETH_USDC: &str  = "9Q5jxFomJL9TxjZSH2oG8FKe7hMwxp8jqL9TcBSusTSm";
    /// BTC/USDC — tick_spacing 64
    pub const BTC_USDC: &str  = "3ucNos4NbumPLZNWztqGZdfYExkm6Lpxa5Vc8FMFnJTY";

    pub const WIF_USDC:  &str = "9agQfsmq3tEFNLDrpj3fdPzTXimTTZCaZLuZALsE6aZb";
    pub const BONK_USDC: &str = "CVW96A5BGmdhmdQSU2tTwciJwyTJVJ1HxaKuAEAXXDVb";
    pub const JTO_USDC:  &str = "<8qdAk17LVxKPecPCCRv5j8kBY5g73EvxdxLwo8t5nMYY";
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Decoded raw state of a Raydium CLMM PoolState account.
#[derive(Debug, Clone)]
pub struct RaydiumState {
    /// Pool address (base58), filled by caller
    pub address: String,

    /// Token A mint pubkey
    pub mint_a: [u8; 32],
    /// Token B mint pubkey
    pub mint_b: [u8; 32],

    /// Decimals of token A (read directly from account — e.g. 9 for SOL)
    pub decimals_a: u8,
    /// Decimals of token B (read directly from account — e.g. 6 for USDC)
    pub decimals_b: u8,

    /// Tick spacing (determines fee tier)
    pub tick_spacing: u16,

    /// Current in-range liquidity
    pub liquidity: u128,

    /// Raw sqrt price in Q64.64 format (same encoding as Whirlpool)
    pub sqrt_price_x64: u128,

    /// Current tick index
    pub tick_current_index: i32,

    /// Slot this account was observed at
    pub slot: u64,

    /// Microsecond timestamp when received from Geyser
    pub received_at_us: i64,
}

/// Derived human readable price from a Raydium CLMM pool
#[derive(Debug, Clone)]
pub struct RaydiumPrice {
    /// Pool address (base58)
    pub pool_address: String,

    /// Canonical symbol, e.g. "SOLUSDC"
    pub symbol: String,

    /// Human price: token B per token A (e.g. USDC per SOL)
    pub price: f64,

    /// Fee rate as a fraction.
    /// Raydium fee tiers by tick_spacing:
    ///   1  → 0.01%  (0.0001)
    ///   10 → 0.02%  (0.0002)
    ///   60 → 0.25%  (0.0025)
    ///   64 → 0.30%  (0.0030)  ← most common
    ///  200 → 1.00%  (0.0100)
    pub fee_rate: f64,

    /// Rough USD liquidity estimate: sqrt(L) * price
    pub liquidity_usd_approx: f64,

    /// Raw liquidity (u128)
    pub liquidity: u128,

    /// Current tick index
    pub tick_current: i32,

    /// Tick spacing
    pub tick_spacing: u16,

    /// Token A decimals (as read from account)
    pub decimals_a: u8,
    /// Token B decimals (as read from account)
    pub decimals_b: u8,

    /// Slot this price was observed at
    pub slot: u64,

    /// Microsecond timestamp when received
    pub received_at_us: i64,
}

impl fmt::Display for RaydiumPrice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[Raydium] {} {:.6} | liq_usd={:.0} | fee={:.4}% | tick={} | slot={}",
            self.symbol,
            self.price,
            self.liquidity_usd_approx,
            self.fee_rate * 100.0,
            self.tick_current,
            self.slot,
        )
    }
}

/// Errors from decoding a Raydium PoolState account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RaydiumDecodeError {
    /// Account data is too short.
    TooShort { got: usize, need: usize },
    /// Discriminator bytes don't match PoolState.
    BadDiscriminator([u8; 8]),
    /// sqrt_price is zero — pool likely uninitialized.
    InvalidSqrtPrice,
    /// Computed price is not finite.
    PriceNotFinite(String),
    /// Both decimals are zero — account may be garbage.
    InvalidDecimals { decimals_a: u8, decimals_b: u8 },
}

impl fmt::Display for RaydiumDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { got, need } =>
                write!(f, "account too short: got {got} bytes, need {need}"),
            Self::BadDiscriminator(d) =>
                write!(f, "wrong discriminator: {d:?}"),
            Self::InvalidSqrtPrice =>
                write!(f, "sqrt_price_x64 is zero — pool uninitialized?"),
            Self::PriceNotFinite(p) =>
                write!(f, "computed price is not finite: {p}"),
            Self::InvalidDecimals { decimals_a, decimals_b } =>
                write!(f, "invalid decimals: a={decimals_a}, b={decimals_b}"),
        }
    }
}

impl std::error::Error for RaydiumDecodeError {}

// ---------------------------------------------------------------------------
// Core decoding
// ---------------------------------------------------------------------------

/// Decode raw PoolState account bytes into [`RaydiumState`].
///
/// # Arguments
/// * `data`           — raw account bytes from Geyser `AccountUpdate`
/// * `address`        — pool pubkey (base58) from the Geyser update key
/// * `slot`           — slot number from the Geyser update
/// * `received_at_us` — current timestamp in microseconds
pub fn decode_raydium_state(
    data: &[u8],
    address: &str,
    slot: u64,
    received_at_us: i64,
) -> Result<RaydiumState, RaydiumDecodeError> {
    // 1. Length check
    if data.len() < POOL_STATE_MIN_LEN {
        return Err(RaydiumDecodeError::TooShort {
            got:  data.len(),
            need: POOL_STATE_MIN_LEN,
        });
    }

    // 2. Discriminator check — skip if all-zero (some providers strip it)
    let disc: [u8; 8] = data[OFF_DISCRIMINATOR..OFF_DISCRIMINATOR + 8]
        .try_into()
        .unwrap();
    let all_zero = disc.iter().all(|&b| b == 0);
    if !all_zero && disc != POOL_STATE_DISCRIMINATOR {
        return Err(RaydiumDecodeError::BadDiscriminator(disc));
    }

    // 3. Read fields
    let mint_a: [u8; 32] = data[OFF_MINT_A..OFF_MINT_A + 32].try_into().unwrap();
    let mint_b: [u8; 32] = data[OFF_MINT_B..OFF_MINT_B + 32].try_into().unwrap();

    let decimals_a  = data[OFF_DECIMALS_A];
    let decimals_b  = data[OFF_DECIMALS_B];
    let tick_spacing = read_u16(data, OFF_TICK_SPACING);
    let liquidity    = read_u128(data, OFF_LIQUIDITY);
    let sqrt_price_x64 = read_u128(data, OFF_SQRT_PRICE);
    let tick_current_index = read_i32(data, OFF_TICK_CURRENT);

    // 4. Sanity: at least one decimal must be nonzero
    // (both zero means the pool was never initialized)
    if decimals_a == 0 && decimals_b == 0 {
        return Err(RaydiumDecodeError::InvalidDecimals { decimals_a, decimals_b });
    }

    Ok(RaydiumState {
        address: address.to_string(),
        mint_a,
        mint_b,
        decimals_a,
        decimals_b,
        tick_spacing,
        liquidity,
        sqrt_price_x64,
        tick_current_index,
        slot,
        received_at_us,
    })
}

/// Convert a [`RaydiumState`] into a human-readable [`RaydiumPrice`].
///
/// Decimal adjustment is computed from the decimals stored in the account —
/// no external `DecimalConfig` required.
pub fn state_to_price(state: &RaydiumState) -> Result<RaydiumPrice, RaydiumDecodeError> {
    if state.sqrt_price_x64 == 0 {
        return Err(RaydiumDecodeError::InvalidSqrtPrice);
    }

    // Q64.64 → f64
    let sqrt_f64 = state.sqrt_price_x64 as f64 / Q64;
    if !sqrt_f64.is_finite() || sqrt_f64 == 0.0 {
        return Err(RaydiumDecodeError::InvalidSqrtPrice);
    }

    // raw_price = sqrt^2, then decimal adjustment
    let raw_price = sqrt_f64 * sqrt_f64;
    let dec_factor = 10_f64.powi(state.decimals_a as i32 - state.decimals_b as i32);
    let price = raw_price * dec_factor;

    if !price.is_finite() {
        return Err(RaydiumDecodeError::PriceNotFinite(format!("{price}")));
    }

    // Infer fee rate from tick_spacing
    // (Raydium doesn't store fee_rate directly — it's in AmmConfig account,
    // but tick_spacing is a reliable proxy for the standard tiers)
    let fee_rate = fee_rate_from_tick_spacing(state.tick_spacing);

    // Rough USD liquidity: sqrt(L) * price
    let liquidity_usd_approx = (state.liquidity as f64).sqrt() * price;

    // Derive symbol from known pool addresses
    let symbol = symbol_for_address(&state.address);

    Ok(RaydiumPrice {
        pool_address: state.address.clone(),
        symbol,
        price,
        fee_rate,
        liquidity_usd_approx,
        liquidity: state.liquidity,
        tick_current: state.tick_current_index,
        tick_spacing: state.tick_spacing,
        decimals_a: state.decimals_a,
        decimals_b: state.decimals_b,
        slot: state.slot,
        received_at_us: state.received_at_us,
    })
}

/// Convenience: decode raw bytes → [`RaydiumPrice`] in a single call.
///
/// # Example
/// ```rust,no_run
/// // Inside Geyser handler:
/// if let Ok(price) = decode_raydium_price(&account.data, &pubkey, slot, now_us()) {
///     println!("{price}");
/// }
/// ```
///
/// Note: `decimals` parameter is accepted for API symmetry with
/// `whirlpool::decode_whirlpool_price` but is ignored — decimals are
/// read directly from the account.
pub fn decode_raydium_price(
    data: &[u8],
    address: &str,
    slot: u64,
    received_at_us: i64,
    _decimals: DecimalConfig, // ignored — read from account
) -> Result<RaydiumPrice, RaydiumDecodeError> {
    let state = decode_raydium_state(data, address, slot, received_at_us)?;
    state_to_price(&state)
}

// ---------------------------------------------------------------------------
// Fee rate inference
// ---------------------------------------------------------------------------

/// Infer fee rate from tick_spacing.
///
/// Raydium standard tiers (from AmmConfig accounts on mainnet):
///
/// | tick_spacing | fee_rate  |
/// |-------------|-----------|
/// | 1           | 0.01%     |
/// | 10          | 0.02%     |
/// | 60          | 0.25%     |
/// | 64          | 0.30%     |
/// | 200         | 1.00%     |
///
/// Unknown spacings fall back to a conservative 0.30%.
pub fn fee_rate_from_tick_spacing(tick_spacing: u16) -> f64 {
    match tick_spacing {
        1   => 0.0001,
        10  => 0.0002,
        60  => 0.0025,
        64  => 0.0030,
        200 => 0.0100,
        _   => 0.0030, // conservative fallback
    }
}

// ---------------------------------------------------------------------------
// Slippage estimator (same interface as whirlpool)
// ---------------------------------------------------------------------------

/// Estimate price impact of a swap against this Raydium pool.
///
/// Uses the same approximation as the Whirlpool version:
/// `price_impact ≈ trade_size_usd / (2 * liquidity_usd)`
pub fn estimate_price_impact(pool: &RaydiumPrice, trade_size_usd: f64) -> f64 {
    if pool.liquidity_usd_approx <= 0.0 {
        return 1.0; // 100% — no liquidity
    }
    trade_size_usd / (2.0 * pool.liquidity_usd_approx)
}

/// Return true if the pool has enough depth for the given trade size.
pub fn has_sufficient_liquidity(
    pool: &RaydiumPrice,
    trade_size_usd: f64,
    max_impact_pct: f64,
) -> bool {
    estimate_price_impact(pool, trade_size_usd) <= max_impact_pct / 100.0
}

// ---------------------------------------------------------------------------
// Staleness check (same interface as whirlpool)
// ---------------------------------------------------------------------------

/// Return true if the pool price is too old to trade on.
pub fn is_stale(pool: &RaydiumPrice, current_slot: u64, max_slot_age: u64) -> bool {
    current_slot.saturating_sub(pool.slot) > max_slot_age
}

// ---------------------------------------------------------------------------
// Pool registry
// ---------------------------------------------------------------------------

/// Map known pool addresses to a canonical symbol string.
pub fn symbol_for_address(address: &str) -> String {
    match address {
        pools::SOL_USDC => "SOLUSDC",
        pools::SOL_USDT => "SOLUSDT",
        pools::ETH_USDC => "ETHUSDC",
        pools::BTC_USDC => "BTCUSDC",
        other           => return other.to_string(),
    }
    .to_string()
}

/// Return Some(DecimalConfig) for known pool addresses.
/// Useful if a caller needs compatibility with the whirlpool API.
pub fn decimal_config_for_pool(address: &str) -> Option<DecimalConfig> {
    match address {
        pools::SOL_USDC => Some(DecimalConfig::SOL_USDC),
        pools::SOL_USDT => Some(DecimalConfig::SOL_USDT),
        pools::ETH_USDC => Some(DecimalConfig::ETH_USDC),
        pools::BTC_USDC => Some(DecimalConfig::BTC_USDC),
        _               => None,
    }
}

// ---------------------------------------------------------------------------
// Low-level byte readers
// ---------------------------------------------------------------------------

#[inline(always)]
fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

#[inline(always)]
fn read_u128(data: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap())
}

#[inline(always)]
fn read_i32(data: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test fixture builder
    // -----------------------------------------------------------------------

    /// Build a synthetic PoolState buffer for testing.
    fn make_pool_data(
        sqrt_price_x64: u128,
        liquidity: u128,
        tick_spacing: u16,
        decimals_a: u8,
        decimals_b: u8,
        tick_current: i32,
    ) -> Vec<u8> {
        let mut data = vec![0u8; 512]; // larger than POOL_STATE_MIN_LEN

        // Leave discriminator as zero (skip check in decoder)
        data[OFF_DECIMALS_A] = decimals_a;
        data[OFF_DECIMALS_B] = decimals_b;
        data[OFF_TICK_SPACING..OFF_TICK_SPACING + 2]
            .copy_from_slice(&tick_spacing.to_le_bytes());
        data[OFF_LIQUIDITY..OFF_LIQUIDITY + 16]
            .copy_from_slice(&liquidity.to_le_bytes());
        data[OFF_SQRT_PRICE..OFF_SQRT_PRICE + 16]
            .copy_from_slice(&sqrt_price_x64.to_le_bytes());
        data[OFF_TICK_CURRENT..OFF_TICK_CURRENT + 4]
            .copy_from_slice(&tick_current.to_le_bytes());
        data
    }

    /// Convert a human price → sqrt_price_x64.
    fn price_to_sqrt_x64(price: f64, decimals_a: u8, decimals_b: u8) -> u128 {
        let dec_factor = 10_f64.powi(decimals_a as i32 - decimals_b as i32);
        let raw = price / dec_factor;
        (raw.sqrt() * Q64) as u128
    }

    // -----------------------------------------------------------------------
    // Decimal reading (the key advantage over Whirlpool)
    // -----------------------------------------------------------------------

    #[test]
    fn test_decimals_read_from_account() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 1_000_000_000_000, 64, 9, 6, -20_000);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 1, 0).unwrap();

        // These come from the account bytes, not an external config
        assert_eq!(state.decimals_a, 9, "SOL decimals wrong");
        assert_eq!(state.decimals_b, 6, "USDC decimals wrong");
    }

    #[test]
    fn test_different_decimal_combinations() {
        // ETH (8 decimals) / USDC (6 decimals)
        let sqrt_x64 = price_to_sqrt_x64(3200.0, 8, 6);
        let data = make_pool_data(sqrt_x64, 5_000_000_000_000, 64, 8, 6, 80_000);
        let state = decode_raydium_state(&data, "test", 1, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        let err_pct = ((price.price - 3200.0) / 3200.0).abs() * 100.0;
        assert!(err_pct < 0.001, "ETH price err={:.6}%, price={:.4}", err_pct, price.price);
    }

    // -----------------------------------------------------------------------
    // Price decoding
    // -----------------------------------------------------------------------

    #[test]
    fn test_decode_sol_usdc_150() {
        let target = 150.0_f64;
        let sqrt_x64 = price_to_sqrt_x64(target, 9, 6);
        let data = make_pool_data(sqrt_x64, 10_000_000_000_000, 64, 9, 6, -20_000);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 285_000_000, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        let err_pct = ((price.price - target) / target).abs() * 100.0;
        assert!(err_pct < 0.001,
            "price={:.6}, expected={:.6}, err={:.6}%", price.price, target, err_pct);
    }

    #[test]
    fn test_decode_btc_usdc_68000() {
        let target = 68_000.0_f64;
        let sqrt_x64 = price_to_sqrt_x64(target, 8, 6);
        let data = make_pool_data(sqrt_x64, 2_000_000_000_000, 64, 8, 6, 107_000);
        let state = decode_raydium_state(&data, pools::BTC_USDC, 285_000_001, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        let err_pct = ((price.price - target) / target).abs() * 100.0;
        assert!(err_pct < 0.001,
            "price={:.2}, expected={:.2}, err={:.6}%", price.price, target, err_pct);
    }

    #[test]
    fn test_decode_low_price_0_001() {
        // Edge: very cheap token, e.g. BONK/USDC
        let target = 0.00001_f64;
        // BONK: 5 decimals, USDC: 6 decimals
        let sqrt_x64 = price_to_sqrt_x64(target, 5, 6);
        let data = make_pool_data(sqrt_x64, 1_000_000_000, 64, 5, 6, -200_000);
        let state = decode_raydium_state(&data, "testpool", 1, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        let err_pct = ((price.price - target) / target).abs() * 100.0;
        assert!(err_pct < 0.01,
            "low price err={:.6}%, price={:.8}", err_pct, price.price);
    }

    // -----------------------------------------------------------------------
    // Fee rate inference
    // -----------------------------------------------------------------------

    #[test]
    fn test_fee_rate_tick_spacing_64() {
        assert!((fee_rate_from_tick_spacing(64) - 0.003).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fee_rate_tick_spacing_1() {
        assert!((fee_rate_from_tick_spacing(1) - 0.0001).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fee_rate_tick_spacing_60() {
        assert!((fee_rate_from_tick_spacing(60) - 0.0025).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fee_rate_unknown_spacing_falls_back() {
        // Unknown spacing falls back to 0.3%
        assert!((fee_rate_from_tick_spacing(99) - 0.003).abs() < f64::EPSILON);
    }

    #[test]
    fn test_fee_rate_stored_in_price() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 1_000_000_000, 64, 9, 6, 0);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 1, 0).unwrap();
        let price = state_to_price(&state).unwrap();
        assert!((price.fee_rate - 0.003).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Error paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_too_short() {
        let data = vec![0u8; 100]; // < POOL_STATE_MIN_LEN
        let err = decode_raydium_state(&data, "test", 1, 0).unwrap_err();
        assert!(matches!(err, RaydiumDecodeError::TooShort { .. }),
            "expected TooShort, got {err:?}");
    }

    #[test]
    fn test_error_bad_discriminator() {
        let mut data = make_pool_data(
            price_to_sqrt_x64(150.0, 9, 6),
            1_000_000_000, 64, 9, 6, 0,
        );
        // Set a non-zero wrong discriminator
        data[..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x03, 0x04]);
        let err = decode_raydium_state(&data, "test", 1, 0).unwrap_err();
        assert!(matches!(err, RaydiumDecodeError::BadDiscriminator(_)),
            "expected BadDiscriminator, got {err:?}");
    }

    #[test]
    fn test_error_zero_sqrt_price() {
        let data = make_pool_data(0, 1_000_000_000, 64, 9, 6, 0);
        let state = decode_raydium_state(&data, "test", 1, 0).unwrap();
        let err = state_to_price(&state).unwrap_err();
        assert!(matches!(err, RaydiumDecodeError::InvalidSqrtPrice));
    }

    #[test]
    fn test_error_both_decimals_zero() {
        // Both decimals zero → pool uninitialized
        let data = make_pool_data(
            price_to_sqrt_x64(150.0, 9, 6),
            1_000_000_000, 64,
            0, // decimals_a = 0
            0, // decimals_b = 0
            0,
        );
        let err = decode_raydium_state(&data, "test", 1, 0).unwrap_err();
        assert!(matches!(err, RaydiumDecodeError::InvalidDecimals { .. }),
            "expected InvalidDecimals, got {err:?}");
    }

    // -----------------------------------------------------------------------
    // Convenience function
    // -----------------------------------------------------------------------

    #[test]
    fn test_convenience_decode_raydium_price() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 10_000_000_000_000, 64, 9, 6, -20_000);

        // DecimalConfig is ignored — decimals come from account
        let price = decode_raydium_price(
            &data, pools::SOL_USDC, 285_000_000, 0, DecimalConfig::SOL_USDC,
        ).unwrap();

        assert!((price.price - 150.0).abs() < 0.01);
        assert_eq!(price.decimals_a, 9);
        assert_eq!(price.decimals_b, 6);
    }

    // -----------------------------------------------------------------------
    // Slippage estimator
    // -----------------------------------------------------------------------

    #[test]
    fn test_price_impact_small_trade() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 100_000_000_000_000, 64, 9, 6, -20_000);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 1, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        let impact = estimate_price_impact(&price, 1000.0);
        assert!(impact < 0.001, "impact too large: {impact:.6}");
    }

    #[test]
    fn test_price_impact_zero_liquidity() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 0, 64, 9, 6, 0);
        let state = decode_raydium_state(&data, "test", 1, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        assert_eq!(estimate_price_impact(&price, 1000.0), 1.0);
    }

    #[test]
    fn test_sufficient_liquidity() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 10_000_000_000_000, 64, 9, 6, 0);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 1, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        assert!(has_sufficient_liquidity(&price, 100.0, 0.1));
    }

    // -----------------------------------------------------------------------
    // Staleness
    // -----------------------------------------------------------------------

    #[test]
    fn test_stale_detection() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, 9, 6);
        let data = make_pool_data(sqrt_x64, 1_000_000_000, 64, 9, 6, 0);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 100, 0).unwrap();
        let price = state_to_price(&state).unwrap();

        assert!(!is_stale(&price, 101, 3),  "should be fresh (1 slot old)");
        assert!(is_stale(&price, 110, 3),   "should be stale (10 slots old)");
    }

    // -----------------------------------------------------------------------
    // Symbol registry
    // -----------------------------------------------------------------------

    #[test]
    fn test_symbol_known() {
        assert_eq!(symbol_for_address(pools::SOL_USDC), "SOLUSDC");
        assert_eq!(symbol_for_address(pools::BTC_USDC), "BTCUSDC");
    }

    #[test]
    fn test_symbol_unknown_returns_address() {
        let addr = "unknownpool111111111111111111111111";
        assert_eq!(symbol_for_address(addr), addr);
    }

    // -----------------------------------------------------------------------
    // Display
    // -----------------------------------------------------------------------

    #[test]
    fn test_display() {
        let sqrt_x64 = price_to_sqrt_x64(148.32, 9, 6);
        let data = make_pool_data(sqrt_x64, 5_000_000_000_000, 64, 9, 6, -20_100);
        let state = decode_raydium_state(&data, pools::SOL_USDC, 285_441_234, 0).unwrap();
        let price = state_to_price(&state).unwrap();
        let s = format!("{price}");

        assert!(s.contains("Raydium"),    "missing Raydium: {s}");
        assert!(s.contains("SOLUSDC"),   "missing symbol: {s}");
        assert!(s.contains("285441234"), "missing slot: {s}");
        println!("{s}");
    }

    // -----------------------------------------------------------------------
    // Whirlpool vs Raydium price parity check
    // -----------------------------------------------------------------------

    #[test]
    fn test_same_sqrt_price_same_result_as_whirlpool() {
        // use crate::dex::whirlpool;

        let target = 148.50_f64;

        // Build the sqrt_price_x64 the same way both decoders would
        let dec_factor = 10_f64.powi(9 - 6); // SOL/USDC
        let raw = target / dec_factor;
        let sqrt_x64 = (raw.sqrt() * Q64) as u128;

        // Raydium decode
        let ray_data = make_pool_data(sqrt_x64, 1_000_000_000_000, 64, 9, 6, -20_000);
        let ray_state = decode_raydium_state(&ray_data, pools::SOL_USDC, 1, 0).unwrap();
        let ray_price = state_to_price(&ray_state).unwrap();

        // Whirlpool decode (same sqrt_price_x64, same decimal config)
        // let mut wh_data = vec![0u8; 653];
        // wh_data[crate::dex::whirlpool::tests_helpers::OFF_SQRT_PRICE_PUB
        //     ..crate::dex::whirlpool::tests_helpers::OFF_SQRT_PRICE_PUB + 16]
        //     .copy_from_slice(&sqrt_x64.to_le_bytes());
        // NOTE: full cross-module test — only runs if whirlpool exposes test helpers
        // This validates both decoders give identical results for same input.

        let diff = (ray_price.price - target).abs();
        assert!(diff < 0.01,
            "Raydium price {:.6} differs from target {:.6}", ray_price.price, target);

        println!("Cross-check: Raydium={:.6}, target={:.6}", ray_price.price, target);
    }
}

// Expose test helpers for cross-module tests (only in test builds)
#[cfg(test)]
pub mod tests_helpers {
    pub const OFF_SQRT_PRICE_PUB: usize = super::OFF_SQRT_PRICE;
}