// // src/dex/pool_monitor.rs

// use yellowstone_grpc_client::GeyserGrpcClient;
// use yellowstone_grpc_proto::prelude::*;

// #[derive(Debug, Clone)]
// pub struct PoolPrice {
//     pub dex: DexKind,
//     pub symbol: String,
//     pub price: Decimal,       // token_b per token_a
//     pub liquidity: u128,      // raw pool liquidity
//     pub slot: u64,
//     pub timestamp_us: i64,
// }

// #[derive(Debug, Clone)]
// pub enum DexKind { Orca, Raydium }

// // Orca Whirlpool accounts to watch
// pub const ORCA_SOL_USDC: &str  = "7qbRF6YsyGuLUVs6Y1q64bdVrfe4ZcUUz1JRdoVNUJnm";
// pub const ORCA_SOL_USDT: &str  = "4fuUiYxTQ6QCrdSq9ouBYcTM7bqSwYTSyLueGZLTy4T4";
// pub const RAYDIUM_SOL_USDC: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWaS5aFMjuFCU";

// pub async fn start_pool_monitor(
//     geyser_url: String,
//     price_tx: tokio::sync::watch::Sender<Option<PoolPrice>>,
// ) -> anyhow::Result<()> {
//     let mut client = GeyserGrpcClient::connect(geyser_url, None, None)?;

//     let accounts = vec![
//         ORCA_SOL_USDC.to_string(),
//         ORCA_SOL_USDT.to_string(),
//         RAYDIUM_SOL_USDC.to_string(),
//     ];

//     let mut stream = client.subscribe_account_updates(accounts).await?;

//     while let Some(update) = stream.next().await {
//         let update = update?;
        
//         if let Some(account) = update.account {
//             let price = match update.pubkey.as_str() {
//                 ORCA_SOL_USDC | ORCA_SOL_USDT => {
//                     decode_whirlpool(&account.data, &update.pubkey)?
//                 }
//                 RAYDIUM_SOL_USDC => {
//                     decode_raydium_clmm(&account.data, &update.pubkey)?
//                 }
//                 _ => continue,
//             };

//             let _ = price_tx.send(Some(price));
//         }
//     }
//     Ok(())
// }

// // Orca Whirlpool price decoder
// // Layout: https://github.com/orca-so/whirlpools/blob/main/programs/whirlpool/src/state/whirlpool.rs
// pub fn decode_whirlpool(data: &[u8], pubkey: &str) -> anyhow::Result<PoolPrice> {
//     // Skip 8-byte discriminator
//     // sqrt_price_x64 is at offset 65 (u128, little-endian)
//     if data.len() < 165 { anyhow::bail!("bad whirlpool data"); }
    
//     let sqrt_price_x64 = u128::from_le_bytes(
//         data[65..81].try_into()?
//     );
//     let liquidity = u128::from_le_bytes(
//         data[81..97].try_into()?
//     );

//     // price = (sqrt_price_x64 / 2^64)^2
//     // For SOL/USDC: adjust for decimals (SOL=9, USDC=6 → factor 10^3)
//     let sqrt = sqrt_price_x64 as f64 / (1u128 << 64) as f64;
//     let raw_price = sqrt * sqrt;
//     let price = raw_price * 1_000f64; // decimal adjustment SOL/USDC

//     let symbol = match pubkey {
//         ORCA_SOL_USDC => "SOLUSDC",
//         ORCA_SOL_USDT => "SOLUSDT",
//         _ => "UNKNOWN",
//     };

//     Ok(PoolPrice {
//         dex: DexKind::Orca,
//         symbol: symbol.to_string(),
//         price: Decimal::from_f64(price).unwrap_or(Decimal::ZERO),
//         liquidity,
//         slot: 0, // filled by caller from update.slot
//         timestamp_us: chrono::Utc::now().timestamp_micros(),
//     })
// }

// // Raydium CLMM price decoder  
// // Layout: https://github.com/raydium-io/raydium-clmm
// pub fn decode_raydium_clmm(data: &[u8], pubkey: &str) -> anyhow::Result<PoolPrice> {
//     if data.len() < 300 { anyhow::bail!("bad raydium data"); }
    
//     // sqrt_price_x64 at offset 197 for CLMM
//     let sqrt_price_x64 = u128::from_le_bytes(
//         data[197..213].try_into()?
//     );
//     let liquidity = u128::from_le_bytes(
//         data[213..229].try_into()?
//     );

//     let sqrt = sqrt_price_x64 as f64 / (1u128 << 64) as f64;
//     let raw_price = sqrt * sqrt * 1_000f64;

//     Ok(PoolPrice {
//         dex: DexKind::Raydium,
//         symbol: "SOLUSDC".to_string(),
//         price: Decimal::from_f64(raw_price).unwrap_or(Decimal::ZERO),
//         liquidity,
//         slot: 0,
//         timestamp_us: chrono::Utc::now().timestamp_micros(),
//     })
// }