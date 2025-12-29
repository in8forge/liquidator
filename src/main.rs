//! # Liquidator V8.0 - Rust Edition
//! 
//! High-performance multi-chain liquidation bot for DeFi protocols.
//! Supports Aave V3, Compound V3, and Venus.

use std::sync::Arc;
use std::path::PathBuf;
use ethers::types::Address;
use ethers::providers::Middleware;
use tokio::signal;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tracing::{info, warn, error, debug, Level};
use tracing_subscriber::EnvFilter;

mod config;
mod chains;
mod protocols;
mod executor;
mod health;
mod discord;
mod types;
mod oracle;
mod borrowers;
mod scanner;
mod swap;

use config::Config;
use chains::ChainManager;
use health::HealthServer;
use oracle::{OracleManager, PriceUpdate};
use borrowers::{BorrowerStore, discover_aave_borrowers};
use discord::Discord;
use scanner::Scanner;

/// How many blocks back to scan for borrowers on first run
const INITIAL_DISCOVERY_BLOCKS: u64 = 500_000;

/// How often to run incremental borrower discovery (5 minutes)
const DISCOVERY_INTERVAL_SECS: u64 = 300;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(Level::INFO.into())
        )
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();

    println!(r#"
╔══════════════════════════════════════════════════════════════════════╗
║  🦀 LIQUIDATOR V8.0 - RUST EDITION                                   ║
║  High-Performance Multi-Chain Liquidation Bot                        ║
╚══════════════════════════════════════════════════════════════════════╝
"#);

    // Load configuration
    let config = Config::from_env()?;
    info!("Configuration loaded");
    
    if config.dry_run {
        warn!("🧪 DRY RUN MODE - No transactions will be sent");
    }

    // Initialize Discord
    let discord = Arc::new(Discord::new(config.discord_webhook.clone()));

    // Initialize chain connections
    let chain_manager = Arc::new(ChainManager::new(&config).await?);
    info!("Chain manager initialized with {} chains", chain_manager.chain_count());

    // Load or discover borrowers
    let borrower_path = PathBuf::from("data/borrowers.json");
    let borrower_store = Arc::new(tokio::sync::RwLock::new(
        load_or_discover_borrowers(&borrower_path, &chain_manager).await
    ));
    
    let total_borrowers = borrower_store.read().await.total_count();
    info!("Total borrowers: {}", total_borrowers);

    // Log per-chain counts
    {
        let store = borrower_store.read().await;
        for chain_ref in chain_manager.chains.iter() {
            let chain_name = chain_ref.key();
            let count = store.get_aave_borrowers(chain_name).len();
            info!("{}: {} borrowers", chain_name, count);
        }
    }

    // Create scanner
    let scanner = Arc::new(Scanner::new(chain_manager.clone(), &config));

    // Create price update channel
    let (price_tx, mut price_rx) = mpsc::channel::<PriceUpdate>(1000);

    // Start health server
    let health_server = HealthServer::new(config.health_port, chain_manager.clone());
    tokio::spawn(async move {
        if let Err(e) = health_server.run().await {
            error!("Health server error: {}", e);
        }
    });
    info!("🏥 Health endpoint: http://localhost:{}/health", config.health_port);

    // Start oracle subscriptions
    let oracle_manager = OracleManager::new(chain_manager.clone(), price_tx);
    tokio::spawn(async move {
        oracle_manager.start().await;
    });

    // Start background monitoring loop
    let cm_clone = chain_manager.clone();
    tokio::spawn(async move {
        cm_clone.start_monitoring().await;
    });

    // Start periodic full scan (every 30 seconds)
    let scanner_periodic = scanner.clone();
    let borrower_store_periodic = borrower_store.clone();
    let chain_manager_periodic = chain_manager.clone();
    tokio::spawn(async move {
        let mut scan_interval = interval(Duration::from_secs(30));
        loop {
            scan_interval.tick().await;
            
            let store = borrower_store_periodic.read().await;
            for chain_ref in chain_manager_periodic.chains.iter() {
                let chain_name = chain_ref.key();
                let borrowers = store.get_aave_borrowers(chain_name);
                
                if borrowers.is_empty() {
                    continue;
                }
                
                debug!("Periodic scan: {} ({} borrowers)", chain_name, borrowers.len());
                let positions = scanner_periodic.scan_chain(chain_name, &borrowers).await;
                
                if !positions.is_empty() {
                    scanner_periodic.process_positions(positions).await;
                }
            }
        }
    });

    // Start incremental borrower discovery (every 5 minutes)
    let borrower_store_discovery = borrower_store.clone();
    let chain_manager_discovery = chain_manager.clone();
    let borrower_path_clone = borrower_path.clone();
    tokio::spawn(async move {
        let mut discovery_interval = interval(Duration::from_secs(DISCOVERY_INTERVAL_SECS));
        loop {
            discovery_interval.tick().await;
            
            let mut store = borrower_store_discovery.write().await;
            let mut total_added = 0;
            
            for chain_ref in chain_manager_discovery.chains.iter() {
                let chain_name = chain_ref.key();
                let chain = chain_ref.value();
                let pool_address: Address = chain.config.pool_address.parse().unwrap_or_default();
                
                match borrowers::update_borrowers(
                    &chain.provider(),
                    pool_address,
                    chain_name,
                    &mut store,
                ).await {
                    Ok(added) => {
                        if added > 0 {
                            info!("{}: Discovered {} new borrowers", chain_name, added);
                            total_added += added;
                        }
                    }
                    Err(e) => {
                        debug!("{}: Discovery error - {}", chain_name, e);
                    }
                }
            }
            
            // Save if we found new borrowers
            if total_added > 0 {
                if let Err(e) = store.save(&borrower_path_clone).await {
                    warn!("Failed to save borrowers: {}", e);
                }
            }
        }
    });

    // Start price event handler (trigger scans on price updates)
    let scanner_price = scanner.clone();
    let borrower_store_price = borrower_store.clone();
    let cm_for_prices = chain_manager.clone();
    tokio::spawn(async move {
        handle_price_updates(
            &mut price_rx, 
            &cm_for_prices, 
            &scanner_price,
            &borrower_store_price,
        ).await;
    });

    // Send startup notification
    let chains_list: Vec<String> = chain_manager.chains.iter()
        .map(|c| c.key().clone())
        .collect();
    discord.send(
        &format!(
            "🦀 LIQUIDATOR V8.0 STARTED\n\
            Mode: {}\n\
            Chains: {}\n\
            Borrowers: {}\n\
            Health: http://localhost:{}",
            if config.dry_run { "DRY RUN" } else { "LIVE" },
            chains_list.join(", "),
            total_borrowers,
            config.health_port
        ),
        true
    ).await;

    info!("🚀 Liquidator running. Press Ctrl+C to stop.");

    // Wait for shutdown signal
    shutdown_signal().await;
    
    info!("Shutting down gracefully...");
    
    // Save borrowers before shutdown
    {
        let store = borrower_store.read().await;
        if let Err(e) = store.save(&borrower_path).await {
            warn!("Failed to save borrowers on shutdown: {}", e);
        } else {
            info!("Saved {} borrowers to disk", store.total_count());
        }
    }
    
    chain_manager.shutdown().await;
    
    // Send shutdown notification
    discord.send("🛑 Liquidator V8.0 shutdown", false).await;
    
    Ok(())
}

/// Load borrowers from disk, or discover them if none exist
async fn load_or_discover_borrowers(
    path: &PathBuf,
    chain_manager: &ChainManager,
) -> BorrowerStore {
    // Try to load existing
    let mut store = BorrowerStore::load(path).await;
    
    if store.total_count() > 0 {
        info!("Loaded {} borrowers from {}", store.total_count(), path.display());
        return store;
    }
    
    // No borrowers - discover them
    info!("🔍 No borrowers found - starting discovery...");
    
    for chain_ref in chain_manager.chains.iter() {
        let chain_name = chain_ref.key();
        let chain = chain_ref.value();
        
        let pool_address: Address = match chain.config.pool_address.parse() {
            Ok(a) => a,
            Err(_) => continue,
        };
        
        let current_block = match chain.provider().get_block_number().await {
            Ok(b) => b.as_u64(),
            Err(e) => {
                error!("{}: Failed to get block number - {}", chain_name, e);
                continue;
            }
        };
        
        let from_block = current_block.saturating_sub(INITIAL_DISCOVERY_BLOCKS);
        
        info!("{}: Discovering borrowers from block {} to {}...", 
            chain_name, from_block, current_block);
        
        match discover_aave_borrowers(
            &chain.provider(),
            pool_address,
            chain_name,
            from_block,
            Some(current_block),
        ).await {
            Ok(borrowers) => {
                let count = borrowers.len();
                store.add_borrowers(chain_name, borrowers);
                store.last_scanned_block.insert(chain_name.clone(), current_block);
                info!("{}: Discovered {} borrowers", chain_name, count);
            }
            Err(e) => {
                error!("{}: Discovery failed - {}", chain_name, e);
            }
        }
    }
    
    // Save discovered borrowers
    if store.total_count() > 0 {
        if let Err(e) = store.save(path).await {
            warn!("Failed to save discovered borrowers: {}", e);
        } else {
            info!("Saved {} borrowers to {}", store.total_count(), path.display());
        }
    }
    
    store
}

/// Handle incoming price updates - trigger targeted scans
async fn handle_price_updates(
    price_rx: &mut mpsc::Receiver<PriceUpdate>,
    chain_manager: &ChainManager,
    scanner: &Scanner,
    borrower_store: &tokio::sync::RwLock<BorrowerStore>,
) {
    while let Some(update) = price_rx.recv().await {
        // Skip if circuit breaker is open
        if chain_manager.is_circuit_open() {
            continue;
        }
        
        debug!(
            "💰 {} price update: ${:.2}",
            update.chain, update.price
        );
        
        chain_manager.stats.write().checks += 1;
        
        // Get borrowers for this chain
        let store = borrower_store.read().await;
        let borrowers = store.get_aave_borrowers(&update.chain);
        
        if borrowers.is_empty() {
            continue;
        }
        
        // Scan positions on this chain
        let positions = scanner.scan_chain(&update.chain, &borrowers).await;
        
        if !positions.is_empty() {
            info!(
                "📊 {} scan: {} positions checked",
                update.chain, positions.len()
            );
            scanner.process_positions(positions).await;
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
