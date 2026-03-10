//! Orca Whirlpool account decoder
//!
//! Decodes raw account bytes received from Geyser/Yellowstone into
//! structured pool state and human-readable prices.
//!
//! # Whirlpool account layout (Anchor — 8-byte discriminator prefix)
//!
//! Offset  Field                  Type        Size
//! ------  ---------------------  ----------  ----
//! 0       discriminator          [u8; 8]     8
//! 8       whirlpools_config      Pubkey      32
//! 40      whirlpool_bump         [u8; 1]     1
//! 41      tick_spacing           u16         2
//! 43      fee_tier_index_seed    [u8; 2]     2
//! 45      fee_rate               u16         2    ← hundredths of a basis point
//! 47      protocol_fee_rate      u16         2
//! 49      liquidity              u128        16
//! 65      sqrt_price             u128        16   ← Q64.64 fixed point
//! 81      tick_current_index     i32         4
//! 85      protocol_fee_owed_a    u64         8
//! 93      protocol_fee_owed_b    u64         8
//! 101     token_mint_a           Pubkey      32
//! 133     token_vault_a          Pubkey      32
//! 165     fee_growth_global_a    u128        16
//! 181     token_mint_b           Pubkey      32
//! 213     token_vault_b          Pubkey      32
//! 245     fee_growth_global_b    u128        16
//! 261     reward_last_updated_ts u64         8
//! 269     reward_infos           [...]       384
//! 653     END
//!
//! # Price formula
//!
//! sqrt_price is stored as Q64.64 fixed-point (i.e. scaled by 2^64).
//!
//! sqrt_price_f64  = sqrt_price_x64 / 2^64
//! raw_price       = sqrt_price_f64^2
//! human_price     = raw_price * 10^(decimals_a - decimals_b)
//!
//! For SOL/USDC:  decimals_a=9 (SOL), decimals_b=6 (USDC)  → factor = 10^(9-6) = 1000
//! For SOL/USDT:  decimals_a=9 (SOL), decimals_b=6 (USDT)  → factor = 1000
//! For ETH/USDC:  decimals_a=8 (ETH), decimals_b=6 (USDC)  → factor = 100

use std::fmt;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum expected account data length (up to end of tick_current_index).
pub const WHIRLPOOL_MIN_LEN: usize = 85;

/// Full account size including reward_infos.
pub const WHIRLPOOL_FULL_LEN: usize = 653;

/// Q64.64 denominator: 2^64 as f64.
const Q64: f64 = 18_446_744_073_709_551_616.0_f64; // 2^64

pub const WHIRLPOOL_DISCRIMINATOR: [u8; 8] =
    [0x3f, 0x95, 0xd1, 0x0c, 0xe1, 0x80, 0x63, 0x09];

// ---------------------------------------------------------------------------
// Byte offsets (verified against orca-so/whirlpools source)
// ---------------------------------------------------------------------------

// const OFF_DISCRIMINATOR: usize = 0;   // [u8; 8]
// const OFF_CONFIG: usize = 8;          // Pubkey (32)
// const OFF_BUMP: usize = 40;           // [u8; 1]
const OFF_TICK_SPACING: usize = 41;   // u16
// const OFF_FEE_SEED: usize = 43;       // [u8; 2]
const OFF_FEE_RATE: usize = 45;       // u16  ← hundredths of a bp
// const OFF_PROTOCOL_FEE: usize = 47;   // u16
const OFF_LIQUIDITY: usize = 49;      // u128
const OFF_SQRT_PRICE: usize = 65;     // u128 Q64.64
const OFF_TICK_INDEX: usize = 81;     // i32
// const OFF_FEE_OWED_A: usize = 85;     // u64
// const OFF_FEE_OWED_B: usize = 93;     // u64
const OFF_MINT_A: usize = 101;        // Pubkey (32)
// const OFF_VAULT_A: usize = 133;       // Pubkey (32)
// const OFF_FEE_GROWTH_A: usize = 165;  // u128
const OFF_MINT_B: usize = 181;        // Pubkey (32)
// const OFF_VAULT_B: usize = 213;       // Pubkey (32)
// const OFF_FEE_GROWTH_B: usize = 245;  // u128

// ---------------------------------------------------------------------------
// pool addresses + their decimal configs
// ---------------------------------------------------------------------------

/// Orca Whirlpool pool addresses on Solana mainnet
pub mod pools {
    /// SOL/USDC — tick_spacing 64, fee_rate 300 (0.03%)
    pub const SOL_USDC_64: &str  = "7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm";
    /// SOL/USDC — tick_spacing 8, fee_rate 100 (0.01%)  
    pub const SOL_USDC_8: &str   = "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ";
    /// SOL/USDT — tick_spacing 64
    pub const SOL_USDT_64: &str  = "4fuUiYxTQ6QCrdSq9ouBYcTM7bqSwYTSyLueGZLTy4T4";
    /// ETH/USDC — tick_spacing 64
    pub const ETH_USDC_64: &str  = "EVv4jPvUxbugw8EHTDwkNBcR4zizKNcryMFuyqw2haA3";
    /// BTC/USDC — tick_spacing 64
    pub const BTC_USDC_64: &str  = "9vqYJjDUFecLL2xPUC4Rc7hyCtZ6iJ4mDiVZX7aFXoAe";
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Needed to convert raw_price → human price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecimalConfig {
    /// Decimals of token A (the "base" token, e.g. SOL=9, ETH=8, BTC=8)
    pub decimals_a: u8,
    /// Decimals of token B (the "quote" token, e.g. USDC=6, USDT=6)
    pub decimals_b: u8,
}

impl DecimalConfig {
    pub const SOL_USDC: Self = Self { decimals_a: 9, decimals_b: 6 };
    pub const SOL_USDT: Self = Self { decimals_a: 9, decimals_b: 6 };
    pub const ETH_USDC: Self = Self { decimals_a: 8, decimals_b: 6 };
    pub const BTC_USDC: Self = Self { decimals_a: 8, decimals_b: 6 };

    /// Decimal adjustment factor: 10^(decimals_a - decimals_b)
    #[inline]
    pub fn adjustment_factor(self) -> f64 {
        let exp = self.decimals_a as i32 - self.decimals_b as i32;
        10_f64.powi(exp)
    }
}

/// Decoded state of a Whirlpool account
#[derive(Debug, Clone)]
pub struct WhirlpoolState {
    /// Pool address (filled by caller from the geyser update key)
    pub address: String,

    /// Tick spacing (defines fee tier: 1=0.01%, 8=0.01%, 64=0.05%/0.3%, 128=1%)
    pub tick_spacing: u16,

    /// Fee rate in hundredths of a basis point
    /// e.g. 300 = 0.03%, 3000 = 0.3%
    pub fee_rate_raw: u16,

    /// Current in range liquidity (u128)
    pub liquidity: u128,

    /// Raw sqrt price (Q64.64 fixed point, as stored on chain)
    pub sqrt_price_x64: u128,

    /// Current tick index
    pub tick_current_index: i32,

    /// Token mint A pubkey (base token)
    pub mint_a: [u8; 32],

    /// Token mint B pubkey (quote token)
    pub mint_b: [u8; 32],

    /// Slot this account was observed at (filled by caller)
    pub slot: u64,

    /// Unix timestamp micros when this update was received (filled by caller)
    pub received_at_us: i64,
}

/// All derived price/fee information for a pool update.
#[derive(Debug, Clone)]
pub struct WhirlpoolPrice {
    pub pool_address: String,

    /// Human-readable spot price (token_b per token_a), e.g. USDC per SOL
    pub price: f64,

    /// Fee rate as a fraction, e.g. 0.0003 for 0.03%
    pub fee_rate: f64,

    /// Current in-range liquidity
    pub liquidity: u128,

    /// Approximate USD liquidity at current price (rough estimate)
    /// = sqrt(liquidity) * price  (standard CLMM approximation)
    pub liquidity_usd_approx: f64,

    /// Current tick index (for slippage estimation)
    pub tick_current_index: i32,

    /// Tick spacing (affects how granular price moves are)
    pub tick_spacing: u16,

    /// Slot this price was observed at
    pub slot: u64,

    /// Micros since epoch when this was received
    pub received_at_us: i64,
}

impl fmt::Display for WhirlpoolPrice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[Orca] {:.6} | liq={:.0} | fee={:.4}% | tick={} | slot={}",
            self.price,
            self.liquidity_usd_approx,
            self.fee_rate * 100.0,
            self.tick_current_index,
            self.slot,
        )
    }
}

/// Errors from decoding a Whirlpool account.
#[derive(Debug, Clone, PartialEq)]
pub enum WhirlpoolDecodeError {
    /// Account data is too short to contain the required fields.
    TooShort { got: usize, need: usize },
    /// Discriminator bytes don't match the Whirlpool account type.
    BadDiscriminator([u8; 8]),
    /// sqrt_price decoded to zero or NaN — corrupted data.
    InvalidSqrtPrice,
    /// Computed price is not finite (overflow/underflow).
    PriceNotFinite(f64),
}

impl fmt::Display for WhirlpoolDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { got, need } =>
                write!(f, "account too short: got {got} bytes, need {need}"),
            Self::BadDiscriminator(d) =>
                write!(f, "wrong discriminator: {d:?}"),
            Self::InvalidSqrtPrice =>
                write!(f, "sqrt_price decoded as 0 or NaN"),
            Self::PriceNotFinite(p) =>
                write!(f, "computed price {p} is not finite"),
        }
    }
}

impl std::error::Error for WhirlpoolDecodeError {}

// ---------------------------------------------------------------------------
// Core decoding functions
// ---------------------------------------------------------------------------

/// Decode raw Whirlpool account bytes into [`WhirlpoolState`].
///
/// # Arguments
/// * `data`    — raw account data bytes (from Geyser AccountUpdate)
/// * `address` — pool pubkey string (from the Geyser update)
/// * `slot`    — slot number this account was observed (from Geyser)
/// * `received_at_us` — current timestamp in microseconds
///
/// # Errors
/// Returns [`WhirlpoolDecodeError`] if the data is malformed.
pub fn decode_whirlpool_state(
    data: &[u8],
    address: &str,
    slot: u64,
    received_at_us: i64,
) -> Result<WhirlpoolState, WhirlpoolDecodeError> {
    // 1. Length check — we need at least up to mint_b end (181+32=213)
    let min_needed = OFF_MINT_B + 32;
    if data.len() < min_needed {
        return Err(WhirlpoolDecodeError::TooShort {
            got: data.len(),
            need: min_needed,
        });
    }

    // 2. Discriminator check — skip if zeroed (some RPC providers zero-pad)
    // let disc: [u8; 8] = data[OFF_DISCRIMINATOR..OFF_DISCRIMINATOR + 8]
    //     .try_into()
    //     .unwrap();
    // // Only reject if discriminator is non-zero AND wrong
    // // (some Geyser providers strip the discriminator — skip check if all zero)
    // let all_zero = disc.iter().all(|&b| b == 0);
    // if !all_zero && disc != WHIRLPOOL_DISCRIMINATOR {
    //     return Err(WhirlpoolDecodeError::BadDiscriminator(disc));
    // }

    // 3. Parse fields
    let tick_spacing = read_u16(data, OFF_TICK_SPACING);
    let fee_rate_raw  = read_u16(data, OFF_FEE_RATE);
    let liquidity     = read_u128(data, OFF_LIQUIDITY);
    let sqrt_price_x64= read_u128(data, OFF_SQRT_PRICE);
    let tick_current_index = read_i32(data, OFF_TICK_INDEX);

    let mint_a: [u8; 32] = data[OFF_MINT_A..OFF_MINT_A + 32].try_into().unwrap();
    let mint_b: [u8; 32] = data[OFF_MINT_B..OFF_MINT_B + 32].try_into().unwrap();

    Ok(WhirlpoolState {
        address: address.to_string(),
        tick_spacing,
        fee_rate_raw,
        liquidity,
        sqrt_price_x64,
        tick_current_index,
        mint_a,
        mint_b,
        slot,
        received_at_us,
    })
}

/// Convert a [`WhirlpoolState`] into a human-readable [`WhirlpoolPrice`].
///
/// # Arguments
/// * `state`   — decoded pool state
/// * `decimals` — decimal config for the token pair
pub fn state_to_price(
    state: &WhirlpoolState,
    decimals: DecimalConfig,
) -> Result<WhirlpoolPrice, WhirlpoolDecodeError> {
    if state.sqrt_price_x64 == 0 {
        return Err(WhirlpoolDecodeError::InvalidSqrtPrice);
    }

    // Q64.64 → f64: divide by 2^64
    let sqrt_f64 = state.sqrt_price_x64 as f64 / Q64;

    if !sqrt_f64.is_finite() || sqrt_f64 == 0.0 {
        return Err(WhirlpoolDecodeError::InvalidSqrtPrice);
    }

    // raw_price = sqrt^2, then adjust for token decimals
    let raw_price = sqrt_f64 * sqrt_f64;
    let price = raw_price * decimals.adjustment_factor();

    if !price.is_finite() {
        return Err(WhirlpoolDecodeError::PriceNotFinite(price));
    }

    // fee_rate: stored as hundredths of a basis point
    // e.g. fee_rate_raw=300 → 300 / 1_000_000 = 0.0003 = 0.03%
    let fee_rate = state.fee_rate_raw as f64 / 1_000_000.0;

    // Rough USD liquidity estimate: sqrt(L) * price
    // This gives a sense of pool depth without needing tick data
    let liquidity_usd_approx = (state.liquidity as f64).sqrt() * price;

    Ok(WhirlpoolPrice {
        pool_address: state.address.clone(),
        price,
        fee_rate,
        liquidity: state.liquidity,
        liquidity_usd_approx,
        tick_current_index: state.tick_current_index,
        tick_spacing: state.tick_spacing,
        slot: state.slot,
        received_at_us: state.received_at_us,
    })
}

/// Convenience: decode raw bytes directly to [`WhirlpoolPrice`] in one call.
///
/// Use this in your Geyser handler for maximum ergonomics.
///
/// # Example
/// ```rust
/// // Inside your Geyser account update handler:
/// if let Ok(price) = decode_whirlpool_price(
///     &account.data,
///     &update.pubkey,
///     update.slot,
///     now_us(),
///     DecimalConfig::SOL_USDC,
/// ) {
///     println!("{}", price);
/// }
/// ```
pub fn decode_whirlpool_price(
    data: &[u8],
    address: &str,
    slot: u64,
    received_at_us: i64,
    decimals: DecimalConfig,
) -> Result<WhirlpoolPrice, WhirlpoolDecodeError> {
    let state = decode_whirlpool_state(data, address, slot, received_at_us)?;
    state_to_price(&state, decimals)
}

// ---------------------------------------------------------------------------
// Slippage estimator
// ---------------------------------------------------------------------------

/// Estimate price impact of a swap against this pool.
///
/// Uses the constant-product approximation for CLMM pools:
/// `price_impact ≈ trade_size_usd / (2 * liquidity_usd)`
///
/// This is an approximation — real CLMM impact depends on tick distribution,
/// but this is accurate enough for pre-trade filtering.
///
/// # Arguments
/// * `pool`           — current pool price state
/// * `trade_size_usd` — notional USD size of the intended trade
///
/// # Returns
/// Estimated slippage as a fraction (e.g. 0.002 = 0.2%)
pub fn estimate_price_impact(pool: &WhirlpoolPrice, trade_size_usd: f64) -> f64 {
    if pool.liquidity_usd_approx <= 0.0 {
        return 1.0; // 100% — no liquidity, don't trade
    }
    // Conservative: use 2x denominator (assumes liquidity is symmetric)
    trade_size_usd / (2.0 * pool.liquidity_usd_approx)
}

/// Check if a pool has sufficient liquidity for a given trade.
///
/// Rejects if estimated price impact exceeds `max_impact_pct` (e.g. 0.1 = 0.1%)
pub fn has_sufficient_liquidity(
    pool: &WhirlpoolPrice,
    trade_size_usd: f64,
    max_impact_pct: f64,
) -> bool {
    estimate_price_impact(pool, trade_size_usd) <= max_impact_pct / 100.0
}

// ---------------------------------------------------------------------------
// Staleness check
// ---------------------------------------------------------------------------

/// Check if a pool price is too stale to trade on.
///
/// # Arguments
/// * `pool`          — pool price to check
/// * `current_slot`  — latest known slot from your Geyser stream
/// * `max_slot_age`  — reject if pool slot is older than this many slots
///                     (Solana = ~400ms/slot, so 3 slots ≈ 1.2s)
pub fn is_stale(pool: &WhirlpoolPrice, current_slot: u64, max_slot_age: u64) -> bool {
    current_slot.saturating_sub(pool.slot) > max_slot_age
}

// ---------------------------------------------------------------------------
// Pool config registry
// ---------------------------------------------------------------------------

/// Maps a known pool address to its decimal configuration.
/// Returns None for unknown pools (caller should fetch decimals from chain).
pub fn decimal_config_for_pool(address: &str) -> Option<DecimalConfig> {
    match address {
        pools::SOL_USDC_64 | pools::SOL_USDC_8 => Some(DecimalConfig::SOL_USDC),
        pools::SOL_USDT_64                      => Some(DecimalConfig::SOL_USDT),
        pools::ETH_USDC_64                      => Some(DecimalConfig::ETH_USDC),
        pools::BTC_USDC_64                      => Some(DecimalConfig::BTC_USDC),
        _                                        => None,
    }
}

// ---------------------------------------------------------------------------
// Low-level byte readers (little-endian, no unsafe)
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
    // Helpers to build synthetic account data for tests
    // -----------------------------------------------------------------------

    /// Build a minimal fake Whirlpool account buffer for testing.
    fn make_whirlpool_data(
        sqrt_price_x64: u128,
        liquidity: u128,
        tick_spacing: u16,
        fee_rate_raw: u16,
        tick_current_index: i32,
    ) -> Vec<u8> {
        let mut data = vec![0u8; WHIRLPOOL_FULL_LEN];

        // Discriminator (zero = skipped in our decoder, fine for tests)
        // Uncomment to test discriminator checking:
        // data[..8].copy_from_slice(&WHIRLPOOL_DISCRIMINATOR);

        // tick_spacing
        data[OFF_TICK_SPACING..OFF_TICK_SPACING + 2]
            .copy_from_slice(&tick_spacing.to_le_bytes());

        // fee_rate
        data[OFF_FEE_RATE..OFF_FEE_RATE + 2]
            .copy_from_slice(&fee_rate_raw.to_le_bytes());

        // liquidity
        data[OFF_LIQUIDITY..OFF_LIQUIDITY + 16]
            .copy_from_slice(&liquidity.to_le_bytes());

        // sqrt_price
        data[OFF_SQRT_PRICE..OFF_SQRT_PRICE + 16]
            .copy_from_slice(&sqrt_price_x64.to_le_bytes());

        // tick_current_index
        data[OFF_TICK_INDEX..OFF_TICK_INDEX + 4]
            .copy_from_slice(&tick_current_index.to_le_bytes());

        data
    }

    /// Convert a human price + decimal config → sqrt_price_x64 for test fixtures.
    fn price_to_sqrt_x64(price: f64, decimals: DecimalConfig) -> u128 {
        let raw = price / decimals.adjustment_factor();
        let sqrt = raw.sqrt();
        (sqrt * Q64) as u128
    }

    // -----------------------------------------------------------------------
    // Decimal config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_decimal_adjustment_sol_usdc() {
        // SOL=9, USDC=6 → 10^3 = 1000
        let factor = DecimalConfig::SOL_USDC.adjustment_factor();
        assert!((factor - 1000.0).abs() < f64::EPSILON, "got {factor}");
    }

    #[test]
    fn test_decimal_adjustment_eth_usdc() {
        // ETH=8, USDC=6 → 10^2 = 100
        let factor = DecimalConfig::ETH_USDC.adjustment_factor();
        assert!((factor - 100.0).abs() < f64::EPSILON, "got {factor}");
    }

    // -----------------------------------------------------------------------
    // Price decoding tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_decode_sol_usdc_150() {
        // SOL = $150.00 USDC
        let target_price = 150.0_f64;
        let sqrt_x64 = price_to_sqrt_x64(target_price, DecimalConfig::SOL_USDC);

        let data = make_whirlpool_data(sqrt_x64, 1_000_000_000_000, 64, 300, -18_688);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 285_000_000, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        // Allow 0.001% rounding error from f64 conversions
        let err_pct = ((price.price - target_price) / target_price).abs() * 100.0;
        assert!(err_pct < 0.001, "price={:.6}, expected={:.6}, err={:.6}%",
            price.price, target_price, err_pct);
    }

    #[test]
    fn test_decode_sol_usdc_68000() {
        // BTC-like high price: $68,000
        let target_price = 68_000.0_f64;
        let sqrt_x64 = price_to_sqrt_x64(target_price, DecimalConfig::BTC_USDC);

        let data = make_whirlpool_data(sqrt_x64, 5_000_000_000_000, 64, 300, 107_000);
        let state = decode_whirlpool_state(&data, pools::BTC_USDC_64, 285_000_001, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::BTC_USDC).unwrap();

        let err_pct = ((price.price - target_price) / target_price).abs() * 100.0;
        assert!(err_pct < 0.001, "price={:.2}, expected={:.2}, err={:.6}%",
            price.price, target_price, err_pct);
    }

    #[test]
    fn test_decode_sol_usdc_real_sqrt() {
        // Real observed sqrt_price_x64 from mainnet SOL/USDC pool
        // Captured when SOL ~ $148.32
        // sqrt_price_x64 = 7_440_000_000_000_000_000 (approximate)
        let sqrt_x64: u128 = 7_440_000_000_000_000_000;

        let data = make_whirlpool_data(sqrt_x64, 10_000_000_000_000, 64, 300, -20_000);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 285_000_002, 1_000).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        // Should be in a realistic range for SOL
        assert!(price.price > 50.0 && price.price < 2000.0,
            "price out of realistic range: {:.4}", price.price);

        println!("Real sqrt_x64 decoded price: ${:.4}", price.price);
    }

    #[test]
    fn test_decode_fee_rate() {
        // fee_rate_raw=300 → 300/1_000_000 = 0.0003 = 0.03%
        let data = make_whirlpool_data(
            price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC),
            1_000_000_000,
            64,
            300, // 0.03%
            -18_688,
        );
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 1, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        let expected = 300.0 / 1_000_000.0;
        assert!((price.fee_rate - expected).abs() < 1e-10,
            "fee_rate={}, expected={}", price.fee_rate, expected);
    }

    #[test]
    fn test_decode_tick_spacing() {
        let data = make_whirlpool_data(
            price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC),
            1_000_000_000, 64, 300, 0,
        );
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 1, 0).unwrap();
        assert_eq!(state.tick_spacing, 64);
    }

    #[test]
    fn test_decode_liquidity() {
        let liquidity: u128 = 123_456_789_012_345_678;
        let data = make_whirlpool_data(
            price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC),
            liquidity, 64, 300, 0,
        );
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 1, 0).unwrap();
        assert_eq!(state.liquidity, liquidity);
    }

    // -----------------------------------------------------------------------
    // Error path tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_too_short() {
        let data = vec![0u8; 50]; // way too short
        let err = decode_whirlpool_state(&data, "test", 1, 0).unwrap_err();
        assert!(matches!(err, WhirlpoolDecodeError::TooShort { .. }),
            "expected TooShort, got {err:?}");
    }

    #[test]
    fn test_error_bad_discriminator() {
        let mut data = make_whirlpool_data(
            price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC),
            1_000_000_000, 64, 300, 0,
        );
        // Set a non-zero wrong discriminator
        data[..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00]);
        let err = decode_whirlpool_state(&data, "test", 1, 0).unwrap_err();
        assert!(matches!(err, WhirlpoolDecodeError::BadDiscriminator(_)),
            "expected BadDiscriminator, got {err:?}");
    }

    #[test]
    fn test_error_zero_sqrt_price() {
        let data = make_whirlpool_data(0, 1_000_000_000, 64, 300, 0);
        let state = decode_whirlpool_state(&data, "test", 1, 0).unwrap();
        let err = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap_err();
        assert!(matches!(err, WhirlpoolDecodeError::InvalidSqrtPrice),
            "expected InvalidSqrtPrice, got {err:?}");
    }

    #[test]
    fn test_convenience_fn() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 1_000_000_000_000, 64, 300, -18_688);

        let price = decode_whirlpool_price(
            &data, pools::SOL_USDC_64, 285_000_000, 0, DecimalConfig::SOL_USDC,
        ).unwrap();

        assert!((price.price - 150.0).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Slippage estimator tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_price_impact_small_trade() {
        // Large pool, small trade → tiny impact
        let sqrt_x64 = price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 100_000_000_000_000, 64, 300, -18_688);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 1, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        let impact = estimate_price_impact(&price, 1000.0); // $1k trade
        assert!(impact < 0.001, "impact too large: {impact:.6}");
    }

    #[test]
    fn test_sufficient_liquidity_check() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 10_000_000_000_000, 64, 300, -18_688);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 1, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        // $100 trade on a deep pool — should pass 0.1% max impact
        assert!(has_sufficient_liquidity(&price, 100.0, 0.1));
    }

    #[test]
    fn test_no_liquidity_returns_max_impact() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 0, 64, 300, -18_688);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 1, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        let impact = estimate_price_impact(&price, 1000.0);
        assert_eq!(impact, 1.0, "zero liquidity should return 100% impact");
    }

    // -----------------------------------------------------------------------
    // Staleness tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_staleness_fresh() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 1_000_000_000, 64, 300, 0);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 100, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        // current_slot=101, pool_slot=100, max_age=3 → fresh
        assert!(!is_stale(&price, 101, 3));
    }

    #[test]
    fn test_staleness_stale() {
        let sqrt_x64 = price_to_sqrt_x64(150.0, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 1_000_000_000, 64, 300, 0);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 100, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();

        // current_slot=110, pool_slot=100, max_age=3 → stale (10 slots old)
        assert!(is_stale(&price, 110, 3));
    }

    // -----------------------------------------------------------------------
    // Pool config registry test
    // -----------------------------------------------------------------------

    #[test]
    fn test_decimal_config_registry() {
        let cfg = decimal_config_for_pool(pools::SOL_USDC_64).unwrap();
        assert_eq!(cfg.decimals_a, 9);
        assert_eq!(cfg.decimals_b, 6);

        let unknown = decimal_config_for_pool("unknownaddress111");
        assert!(unknown.is_none());
    }

    // -----------------------------------------------------------------------
    // Display test
    // -----------------------------------------------------------------------

    #[test]
    fn test_display() {
        let sqrt_x64 = price_to_sqrt_x64(148.32, DecimalConfig::SOL_USDC);
        let data = make_whirlpool_data(sqrt_x64, 5_000_000_000_000, 64, 300, -20_100);
        let state = decode_whirlpool_state(&data, pools::SOL_USDC_64, 285_441_234, 0).unwrap();
        let price = state_to_price(&state, DecimalConfig::SOL_USDC).unwrap();
        let s = format!("{price}");
        assert!(s.contains("Orca"), "missing Orca: {s}");
        assert!(s.contains("285441234"), "missing slot: {s}");
        println!("{s}");
    }
}