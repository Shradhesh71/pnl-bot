use pnl::dex::pool_monitor::{start_pool_monitor, PoolMonitorConfig};
use std::time::Duration;
use tokio::time::timeout;
use tracing::{info, Level};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    info!("Starting pool monitor live test...");

    // Load config from environment variables
    let config = match PoolMonitorConfig::from_env() {
        Ok(cfg) => {
            info!(
                endpoint = %cfg.endpoint,
                pools = cfg.pools.len(),
                "Config loaded from environment"
            );
            cfg
        }
        Err(e) => {
            eprintln!("Failed to load config from environment: {}", e);
            eprintln!("Required env var: GEYSER_ENDPOINT");
            eprintln!("Optional env var: GEYSER_X_TOKEN");
            return Err(e);
        }
    };

    // Start the monitor
    let (handle, mut rx) = start_pool_monitor(config).await?;
    info!("Pool monitor started successfully!");

    println!("\n=== Listening for pool price updates (Ctrl+C to stop) ===\n");

    // Listen for updates with a reasonable timeout
    loop {
        // Use a timeout to periodically show stats
        match timeout(Duration::from_secs(30), rx.changed()).await {
            Ok(Ok(())) => {
                if let Some(price) = &*rx.borrow() {
                    println!("{}", price);
                }
            }
            Ok(Err(_)) => {
                info!("Channel closed, monitor loop ended");
                break;
            }
            Err(_) => {
                // Timeout - show stats
                let updates = handle.updates_received();
                let errors = handle.decode_errors();
                println!(
                    "\n[Stats] Updates received: {}, Decode errors: {}\n",
                    updates, errors
                );
            }
        }
    }

    Ok(())
}
