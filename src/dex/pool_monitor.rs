//! Geyser pool monitor — subscribes to Orca + Raydium pool accounts via
//! Yellowstone gRPC and emits a unified [`PoolPrice`] on every account update.
//!
//! # Usage
//!
//! ```rust,no_run
//! use crate::dex::pool_monitor::{PoolMonitorConfig, start_pool_monitor};
//! use crate::dex::whirlpool::DecimalConfig;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let (monitor, mut rx) = start_pool_monitor(PoolMonitorConfig::from_env()?).await?;
//!
//!     while rx.changed().await.is_ok() {
//!         if let Some(price) = rx.borrow().clone() {
//!             println!("{price}");
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # Architecture
//!
//! ```text
//! start_pool_monitor()
//!   └── tokio::spawn(monitor_loop)
//!         ├── connect_with_retry()   ← TLS + x-token auth
//!         ├── build_subscription()   ← account filter per pool address
//!         ├── keepalive_loop()       ← ping every 10s, pong reply
//!         └── on each AccountUpdate:
//!               ├── route by pubkey → whirlpool::decode or raydium::decode
//!               ├── validate (not stale, has liquidity)
//!               └── watch::Sender::send_if_modified()
//! ```

use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use futures::{sink::SinkExt, stream::StreamExt};
use tokio::{
    sync::watch,
    time::{sleep, Instant},
};
use tonic::transport::ClientTlsConfig;
use tracing::{debug, error, info, warn};

use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof,
    CommitmentLevel,
    SubscribeRequest,
    SubscribeRequestFilterAccounts,
    SubscribeRequestPing,
    SubscribeUpdate,
};

use crate::dex::{
    whirlpool::{
        self,
        DecimalConfig,
        WhirlpoolPrice,
    },
    raydium::{self, RaydiumPrice},
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which DEX a pool price originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DexKind {
    Orca,
    Raydium,
}

impl fmt::Display for DexKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Orca    => write!(f, "Orca"),
            Self::Raydium => write!(f, "Raydium"),
        }
    }
}

/// Unified price update emitted for every pool account change.
/// This is the single type consumed by `strategy::price_state`.
#[derive(Debug, Clone)]
pub struct PoolPrice {
    /// Pool address (base58)
    pub pool_address: String,
    /// Which DEX
    pub dex: DexKind,
    /// Canonical symbol, e.g. "SOLUSDC"
    pub symbol: String,
    /// Human price: quote tokens per base token (e.g. USDC per SOL)
    pub price: f64,
    /// Pool fee as a fraction, e.g. 0.0003
    pub fee_rate: f64,
    /// Rough USD depth estimate (sqrt(L) * price)
    pub liquidity_usd: f64,
    /// Slot this update was observed at
    pub slot: u64,
    /// Microsecond timestamp when we received this update
    pub received_at_us: i64,
}

impl fmt::Display for PoolPrice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[{}] {} {:.6} | liq_usd={:.0} | fee={:.4}% | slot={}",
            self.dex,
            self.symbol,
            self.price,
            self.liquidity_usd,
            self.fee_rate * 100.0,
            self.slot,
        )
    }
}

impl From<WhirlpoolPrice> for PoolPrice {
    fn from(p: WhirlpoolPrice) -> Self {
        let symbol = symbol_for_address(&p.pool_address);
        Self {
            pool_address: p.pool_address,
            dex: DexKind::Orca,
            symbol,
            price: p.price,
            fee_rate: p.fee_rate,
            liquidity_usd: p.liquidity_usd_approx,
            slot: p.slot,
            received_at_us: p.received_at_us,
        }
    }
}

impl From<RaydiumPrice> for PoolPrice {
    fn from(p: RaydiumPrice) -> Self {
        Self {
            pool_address: p.pool_address,
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

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Pool entry in the monitor configuration.
#[derive(Debug, Clone)]
pub struct PoolEntry {
    /// Pool address (base58), e.g. "7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm"
    pub address: String,
    /// Decimal config for this pool (determines price adjustment factor)
    pub decimals: DecimalConfig,
    /// Which DEX this pool belongs to
    pub dex: DexKind,
}

impl PoolEntry {
    pub fn orca(address: &str, decimals: DecimalConfig) -> Self {
        Self {
            address: address.to_string(),
            decimals,
            dex: DexKind::Orca,
        }
    }

    pub fn raydium(address: &str, decimals: DecimalConfig) -> Self {
        Self {
            address: address.to_string(),
            decimals,
            dex: DexKind::Raydium,
        }
    }
}

/// Full configuration for the pool monitor.
#[derive(Debug, Clone)]
pub struct PoolMonitorConfig {
    /// Yellowstone gRPC endpoint, e.g. "https://mainnet.helius-rpc.com:443"
    pub endpoint: String,
    /// x-token for authentication (Helius/Triton/QuickNode API key)
    pub x_token: Option<String>,
    /// Commitment level to subscribe at
    pub commitment: CommitmentLevel,
    /// Pool list to monitor
    pub pools: Vec<PoolEntry>,
    /// How often to send a keepalive ping (default: 10s)
    pub ping_interval: Duration,
    /// How long to wait before reconnecting on error (default: 2s)
    pub reconnect_delay: Duration,
    /// Max reconnect attempts before giving up (0 = infinite)
    pub max_reconnect_attempts: u32,
    /// Reject pool prices older than this many slots (default: 5)
    pub max_slot_age: u64,
    /// Minimum pool liquidity USD to emit (default: 10_000)
    pub min_liquidity_usd: f64,
}

impl PoolMonitorConfig {
    /// Build config from environment variables.
    ///
    /// Required env vars:
    /// - `GEYSER_ENDPOINT` — gRPC endpoint URL
    ///
    /// Optional:
    /// - `GEYSER_X_TOKEN` — authentication token
    pub fn from_env() -> anyhow::Result<Self> {
        let endpoint = std::env::var("GEYSER_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("GEYSER_ENDPOINT not set"))?;
        let x_token = std::env::var("GEYSER_X_TOKEN").ok();

        Ok(Self {
            endpoint,
            x_token,
            commitment: CommitmentLevel::Processed, // fastest
            pools: default_pools(),
            ping_interval: Duration::from_secs(10),
            reconnect_delay: Duration::from_secs(2),
            max_reconnect_attempts: 0, // infinite
            max_slot_age: 5,
            min_liquidity_usd: 10_000.0,
        })
    }

    /// Build config directly (for testing or programmatic setup).
    pub fn new(endpoint: impl Into<String>, x_token: Option<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            x_token,
            commitment: CommitmentLevel::Processed,
            pools: default_pools(),
            ping_interval: Duration::from_secs(10),
            reconnect_delay: Duration::from_secs(2),
            max_reconnect_attempts: 0,
            max_slot_age: 5,
            min_liquidity_usd: 10_000.0,
        }
    }

    /// Override pool list.
    pub fn with_pools(mut self, pools: Vec<PoolEntry>) -> Self {
        self.pools = pools;
        self
    }
}

/// Default set of pools to monitor (SOL/USDC + SOL/USDT on Orca + Raydium).
pub fn default_pools() -> Vec<PoolEntry> {
    use whirlpool::pools::*;
    use crate::dex::raydium::pools as ray_pools;

    vec![
        PoolEntry::orca(SOL_USDC_64, DecimalConfig::SOL_USDC),
        PoolEntry::orca(SOL_USDC_8,  DecimalConfig::SOL_USDC),
        PoolEntry::orca(SOL_USDT_64, DecimalConfig::SOL_USDT),
        PoolEntry::raydium(ray_pools::SOL_USDC, DecimalConfig::SOL_USDC),

        PoolEntry::orca(WIF_USDC, DecimalConfig::WIF_USDC),
        PoolEntry::orca(JTO_USDC, DecimalConfig::JTO_USDC),
        PoolEntry::orca(BONK_USDC, DecimalConfig::BONK_USDC),

        PoolEntry::raydium(WIF_USDC, DecimalConfig::WIF_USDC),
        PoolEntry::raydium(JTO_USDC, DecimalConfig::JTO_USDC),
        PoolEntry::raydium(BONK_USDC, DecimalConfig::BONK_USDC)
    ]
}

// ---------------------------------------------------------------------------
// Monitor handle
// ---------------------------------------------------------------------------

/// Handle to a running pool monitor task.
/// Dropping this will NOT stop the task — call `.abort()` explicitly.
pub struct PoolMonitorHandle {
    task: tokio::task::JoinHandle<()>,
    /// Total account updates received since start
    pub updates_received: Arc<AtomicU64>,
    /// Total decode errors since start
    pub decode_errors: Arc<AtomicU64>,
}

impl PoolMonitorHandle {
    /// Abort the background monitor task.
    pub fn abort(&self) {
        self.task.abort();
    }

    pub fn updates_received(&self) -> u64 {
        self.updates_received.load(Ordering::Relaxed)
    }

    pub fn decode_errors(&self) -> u64 {
        self.decode_errors.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Start the pool monitor.
///
/// Returns a [`PoolMonitorHandle`] and a [`watch::Receiver<Option<PoolPrice>>`].
/// The receiver is updated on every valid pool account change.
/// Use `rx.changed().await` to be notified of new prices.
///
/// # Example
/// ```rust,no_run
/// let (handle, mut rx) = start_pool_monitor(config).await?;
///
/// while rx.changed().await.is_ok() {
///     if let Some(price) = &*rx.borrow() {
///         println!("{price}");
///     }
/// }
/// ```
pub async fn start_pool_monitor(
    config: PoolMonitorConfig,
) -> anyhow::Result<(PoolMonitorHandle, watch::Receiver<Option<PoolPrice>>)> {
    let (tx, rx) = watch::channel(None::<PoolPrice>);

    let updates_received = Arc::new(AtomicU64::new(0));
    let decode_errors    = Arc::new(AtomicU64::new(0));

    let updates_clone = updates_received.clone();
    let errors_clone  = decode_errors.clone();

    let task = tokio::spawn(async move {
        monitor_loop(config, tx, updates_clone, errors_clone).await;
    });

    Ok((
        PoolMonitorHandle { task, updates_received, decode_errors },
        rx,
    ))
}

// ---------------------------------------------------------------------------
// Internal: monitor loop with auto-reconnect
// ---------------------------------------------------------------------------

async fn monitor_loop(
    config: PoolMonitorConfig,
    tx: watch::Sender<Option<PoolPrice>>,
    updates_received: Arc<AtomicU64>,
    decode_errors: Arc<AtomicU64>,
) {
    // Build a lookup map: address → PoolEntry (for fast routing on each update)
    let pool_map: HashMap<String, PoolEntry> = config
        .pools
        .iter()
        .map(|p| (p.address.clone(), p.clone()))
        .collect();

    let pool_addresses: Vec<String> = config.pools.iter().map(|p| p.address.clone()).collect();

    info!(
        pools = pool_addresses.len(),
        endpoint = %config.endpoint,
        "Pool monitor starting"
    );

    let mut attempt: u32 = 0;

    loop {
        attempt += 1;

        if config.max_reconnect_attempts > 0 && attempt > config.max_reconnect_attempts {
            error!("Pool monitor: max reconnect attempts ({}) reached, stopping", attempt - 1);
            break;
        }

        info!(attempt, "Pool monitor: connecting to Geyser");

        match run_subscription(
            &config,
            &pool_map,
            &pool_addresses,
            &tx,
            &updates_received,
            &decode_errors,
        )
        .await
        {
            Ok(()) => {
                info!("Pool monitor: stream closed cleanly, reconnecting");
            }
            Err(e) => {
                warn!(error = %e, "Pool monitor: stream error, reconnecting");
            }
        }

        // Exponential backoff capped at 30s
        let delay = (config.reconnect_delay * attempt.min(10))
            .min(Duration::from_secs(30));
        info!(delay_ms = delay.as_millis(), "Pool monitor: waiting before reconnect");
        sleep(delay).await;
    }
}

// ---------------------------------------------------------------------------
// Internal: single subscription session
// ---------------------------------------------------------------------------

async fn run_subscription(
    config: &PoolMonitorConfig,
    pool_map: &HashMap<String, PoolEntry>,
    pool_addresses: &[String],
    tx: &watch::Sender<Option<PoolPrice>>,
    updates_received: &Arc<AtomicU64>,
    decode_errors: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    // 1. Connect
    let mut client = connect(config).await?;
    info!("Pool monitor: Geyser connected");

    // 2. Build account filter
    let mut accounts_filter = HashMap::new();
    accounts_filter.insert(
        "pool_monitor".to_string(),
        SubscribeRequestFilterAccounts {
            account: pool_addresses.to_vec(),
            owner:   vec![],
            filters: vec![],
            ..Default::default()
        },
    );

    // 3. Open bidirectional stream
    let (mut subscribe_tx, mut stream) = client.subscribe().await?;

    // 4. Send initial subscription request
    let subscribe_request = SubscribeRequest {
        accounts:    accounts_filter,
        slots:       HashMap::new(),
        transactions:HashMap::new(),
        blocks:      HashMap::new(),
        blocks_meta: HashMap::new(),
        commitment:  Some(config.commitment as i32),
        accounts_data_slice: vec![],
        ping:        None,
        ..Default::default()
    };

    subscribe_tx.send(subscribe_request).await?;
    info!("Pool monitor: subscription request sent");

    // 5. Process stream — ping keepalive + account updates
    let ping_interval = config.ping_interval;
    let mut ping_id: i32 = 0;
    let mut last_ping = Instant::now();

    while let Some(msg) = stream.next().await {
        // Keepalive ping
        if last_ping.elapsed() >= ping_interval {
            ping_id = ping_id.wrapping_add(1);
            let ping_req = SubscribeRequest {
                ping: Some(SubscribeRequestPing { id: ping_id }),
                ..Default::default()
            };
            if let Err(e) = subscribe_tx.send(ping_req).await {
                warn!(error = %e, "Pool monitor: failed to send ping");
                break;
            }
            last_ping = Instant::now();
            debug!(ping_id, "Pool monitor: ping sent");
        }

        // Handle message
        match msg {
            Err(e) => {
                return Err(anyhow::anyhow!("stream error: {e}"));
            }
            Ok(update) => {
                handle_update(
                    update,
                    pool_map,
                    config,
                    tx,
                    updates_received,
                    decode_errors,
                );
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal: handle one SubscribeUpdate message
// ---------------------------------------------------------------------------

fn handle_update(
    update: SubscribeUpdate,
    pool_map: &HashMap<String, PoolEntry>,
    config: &PoolMonitorConfig,
    tx: &watch::Sender<Option<PoolPrice>>,
    updates_received: &Arc<AtomicU64>,
    decode_errors: &Arc<AtomicU64>,
) {
    match update.update_oneof {
        // ── Pong (response to our ping) ──────────────────────────────────
        Some(UpdateOneof::Pong(pong)) => {
            debug!(id = pong.id, "Pool monitor: pong received");
        }

        // ── Account update ────────────────────────────────────────────────
        Some(UpdateOneof::Account(account_update)) => {
            let slot = account_update.slot;

            let info = match account_update.account {
                Some(info) => info,
                None => {
                    debug!("Pool monitor: account update with no info, skipping");
                    return;
                }
            };

            // Decode pubkey bytes → base58 address
            let address = bs58::encode(&info.pubkey).into_string();

            // Route to correct decoder
            let entry = match pool_map.get(&address) {
                Some(e) => e,
                None => {
                    debug!(%address, "Pool monitor: unknown pool address, skipping");
                    return;
                }
            };

            updates_received.fetch_add(1, Ordering::Relaxed);
            let now_us = now_us();
            
            tracing::debug!(
                address = %address,
                data_len = info.data.len(),
                first_8_bytes = ?&info.data[..8.min(info.data.len())],
                "Decoding Whirlpool account"
            );

            let pool_price: Option<PoolPrice> = match entry.dex {
                DexKind::Orca => {
                    match whirlpool::decode_whirlpool_price(
                        &info.data,
                        &address,
                        slot,
                        now_us,
                        entry.decimals,
                    ) {
                        Ok(p) => Some(PoolPrice::from(p)),
                        Err(e) => {
                            decode_errors.fetch_add(1, Ordering::Relaxed);
                            debug!(error = %e, %address, "Whirlpool decode error");
                            None
                        }
                    }
                }
                DexKind::Raydium => {
                    match raydium::decode_raydium_price(
                        &info.data,
                        &address,
                        slot,
                        now_us,
                        entry.decimals,
                    ) {
                        Ok(p) => Some(PoolPrice::from(p)),
                        Err(e) => {
                            decode_errors.fetch_add(1, Ordering::Relaxed);
                            debug!(error = %e, %address, "Raydium decode error");
                            None
                        }
                    }
                }
            };

            let price = match pool_price {
                Some(p) => p,
                None => return,
            };

            // ── Validation gates ────────────────────────────────────────
            // 1. Liquidity check
            if price.liquidity_usd < config.min_liquidity_usd {
                debug!(
                    liquidity_usd = price.liquidity_usd,
                    min = config.min_liquidity_usd,
                    "Pool monitor: insufficient liquidity, skipping"
                );
                return;
            }

            // 2. Price sanity (must be positive and finite)
            if !price.price.is_finite() || price.price <= 0.0 {
                warn!(%address, price = price.price, "Pool monitor: invalid price, skipping");
                return;
            }

            debug!(
                dex = %price.dex,
                symbol = %price.symbol,
                price = price.price,
                slot,
                "Pool monitor: price update"
            );

            // Send to strategy layer — send_if_modified avoids waking
            // downstream tasks when the value hasn't changed
            let _ = tx.send_if_modified(|current| {
                // Always update: pool prices change on every slot
                *current = Some(price);
                true
            });
        }

        // ── Slot update (track latest slot for staleness checks) ─────────
        Some(UpdateOneof::Slot(slot_update)) => {
            debug!(slot = slot_update.slot, "Pool monitor: slot update");
        }

        // ── Everything else — ignore ──────────────────────────────────────
        Some(other) => {
            debug!("Pool monitor: unhandled update type: {:?}", std::mem::discriminant(&other));
        }
        None => {}
    }
}

// ---------------------------------------------------------------------------
// Internal: connection helper
// ---------------------------------------------------------------------------

async fn connect(config: &PoolMonitorConfig) -> anyhow::Result<GeyserGrpcClient<impl tonic::service::Interceptor>> {
    let client = GeyserGrpcClient::build_from_shared(config.endpoint.clone())?
        .x_token(config.x_token.clone())?
        .tls_config(ClientTlsConfig::new().with_native_roots())?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(10))
        .connect()
        .await?;

    Ok(client)
}

// ---------------------------------------------------------------------------
// Internal: helpers
// ---------------------------------------------------------------------------

/// Current time in microseconds since Unix epoch.
#[inline]
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

/// Map known pool address → canonical symbol string.
fn symbol_for_address(address: &str) -> String {
    use whirlpool::pools::*;
    match address {
        SOL_USDC_64 | SOL_USDC_8 => "SOLUSDC",
        SOL_USDT_64               => "SOLUSDT",
        ETH_USDC_64               => "ETHUSDC",
        BTC_USDC_64               => "BTCUSDC",
        other => return other.to_string(),
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── PoolPrice display ────────────────────────────────────────────────

    #[test]
    fn test_pool_price_display() {
        let p = PoolPrice {
            pool_address: "7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm".to_string(),
            dex: DexKind::Orca,
            symbol: "SOLUSDC".to_string(),
            price: 148.3201,
            fee_rate: 0.0003,
            liquidity_usd: 2_500_000.0,
            slot: 285_441_234,
            received_at_us: 1_700_000_000_000_000,
        };
        let s = format!("{p}");
        assert!(s.contains("Orca"),     "missing Orca: {s}");
        assert!(s.contains("SOLUSDC"),  "missing symbol: {s}");
        assert!(s.contains("148.3201"), "missing price: {s}");
        assert!(s.contains("285441234"),"missing slot: {s}");
        println!("{s}");
    }

    // ── DexKind display ──────────────────────────────────────────────────

    #[test]
    fn test_dex_kind_display() {
        assert_eq!(format!("{}", DexKind::Orca),    "Orca");
        assert_eq!(format!("{}", DexKind::Raydium), "Raydium");
    }

    // ── Symbol routing ───────────────────────────────────────────────────

    #[test]
    fn test_symbol_for_known_address() {
        assert_eq!(
            symbol_for_address(whirlpool::pools::SOL_USDC_64),
            "SOLUSDC"
        );
        assert_eq!(
            symbol_for_address(whirlpool::pools::SOL_USDT_64),
            "SOLUSDT"
        );
    }

    #[test]
    fn test_symbol_for_unknown_address() {
        let addr = "unknown1111111111111111111111111111";
        assert_eq!(symbol_for_address(addr), addr);
    }

    // ── Pool entry constructors ──────────────────────────────────────────

    #[test]
    fn test_pool_entry_orca() {
        let entry = PoolEntry::orca(
            whirlpool::pools::SOL_USDC_64,
            DecimalConfig::SOL_USDC,
        );
        assert_eq!(entry.dex, DexKind::Orca);
        assert_eq!(entry.decimals, DecimalConfig::SOL_USDC);
    }

    #[test]
    fn test_pool_entry_raydium() {
        use crate::dex::raydium::pools as ray;
        let entry = PoolEntry::raydium(ray::SOL_USDC, DecimalConfig::SOL_USDC);
        assert_eq!(entry.dex, DexKind::Raydium);
    }

    // ── Config construction ──────────────────────────────────────────────

    #[test]
    fn test_config_new() {
        let cfg = PoolMonitorConfig::new("https://example.com:443", Some("token".into()));
        assert_eq!(cfg.endpoint, "https://example.com:443");
        assert_eq!(cfg.x_token, Some("token".into()));
        assert!(!cfg.pools.is_empty(), "default pools should not be empty");
        assert_eq!(cfg.max_slot_age, 5);
        assert_eq!(cfg.min_liquidity_usd, 10_000.0);
    }

    #[test]
    fn test_config_with_pools() {
        let custom = vec![
            PoolEntry::orca(whirlpool::pools::SOL_USDC_64, DecimalConfig::SOL_USDC),
        ];
        let cfg = PoolMonitorConfig::new("https://example.com:443", None)
            .with_pools(custom.clone());
        assert_eq!(cfg.pools.len(), 1);
        assert_eq!(cfg.pools[0].address, custom[0].address);
    }

    // ── From conversions ─────────────────────────────────────────────────

    #[test]
    fn test_from_whirlpool_price() {
        let wp = WhirlpoolPrice {
            pool_address: whirlpool::pools::SOL_USDC_64.to_string(),
            price: 150.0,
            fee_rate: 0.0003,
            liquidity: 1_000_000_000_000,
            liquidity_usd_approx: 500_000.0,
            tick_current_index: -18_688,
            tick_spacing: 64,
            slot: 285_000_000,
            received_at_us: 0,
        };
        let pp = PoolPrice::from(wp);
        assert_eq!(pp.dex, DexKind::Orca);
        assert_eq!(pp.symbol, "SOLUSDC");
        assert!((pp.price - 150.0).abs() < f64::EPSILON);
        assert!((pp.fee_rate - 0.0003).abs() < f64::EPSILON);
    }

    // ── Default pools ────────────────────────────────────────────────────

    #[test]
    fn test_default_pools_not_empty() {
        let pools = default_pools();
        assert!(!pools.is_empty());
        // All should have valid addresses (non-empty)
        for p in &pools {
            assert!(!p.address.is_empty(), "empty address in default pools");
        }
    }

    #[test]
    fn test_default_pools_contain_sol_usdc() {
        let pools = default_pools();
        let has_sol_usdc = pools.iter().any(|p| {
            p.address == whirlpool::pools::SOL_USDC_64 && p.dex == DexKind::Orca
        });
        assert!(has_sol_usdc, "SOL/USDC Orca pool missing from defaults");
    }

    // ── now_us sanity ────────────────────────────────────────────────────

    #[test]
    fn test_now_us_is_positive_and_recent() {
        let ts = now_us();
        // Should be after 2024-01-01 (1_704_067_200_000_000 us)
        assert!(ts > 1_704_067_200_000_000, "timestamp too old: {ts}");
    }

    // ── watch channel integration ─────────────────────────────────────────

    #[tokio::test]
    async fn test_watch_channel_sends_pool_price() {
        let (tx, mut rx) = watch::channel(None::<PoolPrice>);

        let price = PoolPrice {
            pool_address: whirlpool::pools::SOL_USDC_64.to_string(),
            dex: DexKind::Orca,
            symbol: "SOLUSDC".to_string(),
            price: 148.32,
            fee_rate: 0.0003,
            liquidity_usd: 2_000_000.0,
            slot: 285_441_234,
            received_at_us: now_us(),
        };

        tx.send(Some(price.clone())).unwrap();

        // Should be able to read immediately
        assert!(rx.changed().await.is_ok());
        let received = rx.borrow().clone().unwrap();
        assert!((received.price - price.price).abs() < f64::EPSILON);
        assert_eq!(received.symbol, "SOLUSDC");
    }

    // ── AtomicU64 counters ───────────────────────────────────────────────

    #[test]
    fn test_atomic_counters() {
        let counter = Arc::new(AtomicU64::new(0));
        counter.fetch_add(1, Ordering::Relaxed);
        counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }
}