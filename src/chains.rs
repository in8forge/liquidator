//! Chain management with multi-RPC failover for crash resilience.
//!
//! Each chain supports multiple RPC endpoints with automatic failover:
//! - Health checks per RPC
//! - Auto-rotate on failure
//! - Weighted selection based on latency

use ethers::prelude::*;
use ethers::providers::{Provider, Http, Middleware};
use ethers::signers::LocalWallet;
use ethers::types::Address;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{info, warn, error, debug};

use crate::config::{Config, ChainConfig};
use crate::types::*;

/// Multicall3 address (same on all chains)
pub const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// Single RPC endpoint with health tracking
pub struct RpcEndpoint {
    pub url: String,
    pub provider: Provider<Http>,
    pub failures: AtomicUsize,
    pub successes: AtomicUsize,
    pub last_latency_ms: AtomicU64,
    pub is_healthy: RwLock<bool>,
    pub last_check: RwLock<Instant>,
}

impl RpcEndpoint {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let provider = Provider::<Http>::try_from(url)?;
        
        Ok(Self {
            url: url.to_string(),
            provider,
            failures: AtomicUsize::new(0),
            successes: AtomicUsize::new(0),
            last_latency_ms: AtomicU64::new(0),
            is_healthy: RwLock::new(true),
            last_check: RwLock::new(Instant::now()),
        })
    }
    
    pub fn record_success(&self, latency_ms: u64) {
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.failures.store(0, Ordering::Relaxed); // Reset failures on success
        self.last_latency_ms.store(latency_ms, Ordering::Relaxed);
        *self.is_healthy.write() = true;
    }
    
    pub fn record_failure(&self) {
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        
        // Mark unhealthy after 3 consecutive failures
        if failures >= 3 {
            *self.is_healthy.write() = false;
            warn!("RPC {} marked unhealthy after {} failures", self.url, failures);
        }
    }
    
    pub fn is_healthy(&self) -> bool {
        *self.is_healthy.read()
    }
    
    /// Health check - try to get block number
    pub async fn health_check(&self) -> bool {
        let start = Instant::now();
        
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.provider.get_block_number(),
        ).await {
            Ok(Ok(_)) => {
                let latency = start.elapsed().as_millis() as u64;
                self.record_success(latency);
                *self.last_check.write() = Instant::now();
                true
            }
            _ => {
                self.record_failure();
                *self.last_check.write() = Instant::now();
                false
            }
        }
    }
}

/// Multi-RPC provider with automatic failover
pub struct MultiRpcProvider {
    pub endpoints: Vec<Arc<RpcEndpoint>>,
    pub current_index: AtomicUsize,
    pub chain_name: String,
}

impl MultiRpcProvider {
    pub fn new(chain_name: &str, rpc_urls: &[String]) -> anyhow::Result<Self> {
        let mut endpoints = Vec::new();
        
        for url in rpc_urls {
            match RpcEndpoint::new(url) {
                Ok(endpoint) => {
                    endpoints.push(Arc::new(endpoint));
                }
                Err(e) => {
                    warn!("{}: Failed to create RPC endpoint {}: {}", chain_name, url, e);
                }
            }
        }
        
        if endpoints.is_empty() {
            anyhow::bail!("No valid RPC endpoints for {}", chain_name);
        }
        
        info!("{}: Initialized with {} RPC endpoints", chain_name, endpoints.len());
        
        Ok(Self {
            endpoints,
            current_index: AtomicUsize::new(0),
            chain_name: chain_name.to_string(),
        })
    }
    
    /// Get the current best provider
    pub fn get_provider(&self) -> &Provider<Http> {
        let idx = self.current_index.load(Ordering::Relaxed) % self.endpoints.len();
        &self.endpoints[idx].provider
    }
    
    /// Get provider, rotating to next healthy one if current fails
    pub fn get_healthy_provider(&self) -> &Provider<Http> {
        let start_idx = self.current_index.load(Ordering::Relaxed);
        let len = self.endpoints.len();
        
        // Try to find a healthy endpoint
        for i in 0..len {
            let idx = (start_idx + i) % len;
            if self.endpoints[idx].is_healthy() {
                if i > 0 {
                    // We rotated, update the index
                    self.current_index.store(idx, Ordering::Relaxed);
                    info!("{}: Rotated to RPC #{} ({})", 
                        self.chain_name, idx, self.endpoints[idx].url);
                }
                return &self.endpoints[idx].provider;
            }
        }
        
        // All unhealthy, return current anyway (maybe it recovered)
        warn!("{}: All RPCs unhealthy, using current", self.chain_name);
        &self.endpoints[start_idx % len].provider
    }
    
    /// Record success for current provider
    pub fn record_success(&self, latency_ms: u64) {
        let idx = self.current_index.load(Ordering::Relaxed) % self.endpoints.len();
        self.endpoints[idx].record_success(latency_ms);
    }
    
    /// Record failure and potentially rotate
    pub fn record_failure(&self) {
        let idx = self.current_index.load(Ordering::Relaxed) % self.endpoints.len();
        self.endpoints[idx].record_failure();
        
        // If current is now unhealthy, rotate
        if !self.endpoints[idx].is_healthy() {
            self.rotate_to_next_healthy();
        }
    }
    
    /// Rotate to next healthy endpoint
    fn rotate_to_next_healthy(&self) {
        let start_idx = self.current_index.load(Ordering::Relaxed);
        let len = self.endpoints.len();
        
        for i in 1..len {
            let idx = (start_idx + i) % len;
            if self.endpoints[idx].is_healthy() {
                self.current_index.store(idx, Ordering::Relaxed);
                info!("{}: Failover to RPC #{} ({})", 
                    self.chain_name, idx, self.endpoints[idx].url);
                return;
            }
        }
        
        // No healthy endpoints, try to reset and use first one
        warn!("{}: No healthy RPCs, resetting all to healthy", self.chain_name);
        for endpoint in &self.endpoints {
            *endpoint.is_healthy.write() = true;
            endpoint.failures.store(0, Ordering::Relaxed);
        }
        self.current_index.store(0, Ordering::Relaxed);
    }
    
    /// Run health checks on all endpoints
    pub async fn health_check_all(&self) {
        for (i, endpoint) in self.endpoints.iter().enumerate() {
            let healthy = endpoint.health_check().await;
            debug!("{}: RPC #{} ({}) - {}", 
                self.chain_name, i, endpoint.url,
                if healthy { "healthy" } else { "unhealthy" });
        }
    }
    
    /// Get status of all endpoints
    pub fn status(&self) -> Vec<serde_json::Value> {
        self.endpoints.iter().enumerate().map(|(i, ep)| {
            let current = self.current_index.load(Ordering::Relaxed) % self.endpoints.len();
            serde_json::json!({
                "index": i,
                "url": ep.url,
                "healthy": ep.is_healthy(),
                "active": i == current,
                "failures": ep.failures.load(Ordering::Relaxed),
                "successes": ep.successes.load(Ordering::Relaxed),
                "latency_ms": ep.last_latency_ms.load(Ordering::Relaxed),
            })
        }).collect()
    }
}

/// Chain-specific state and connections
pub struct ChainState {
    pub config: ChainConfig,
    pub multi_rpc: MultiRpcProvider,
    pub wallet: LocalWallet,
    pub prices: DashMap<Address, PriceData>,
    pub nonce: RwLock<u64>,
}

impl ChainState {
    pub async fn new(config: ChainConfig, private_key: &str) -> anyhow::Result<Self> {
        // Parse RPC URLs (comma-separated or single)
        let rpc_urls: Vec<String> = config.rpc_url
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        
        let multi_rpc = MultiRpcProvider::new(&config.name, &rpc_urls)?;
        
        let pk_clean = private_key.strip_prefix("0x").unwrap_or(private_key);
        let wallet: LocalWallet = pk_clean.parse()?;
        
        // Get initial nonce from primary provider
        let nonce = multi_rpc.get_provider()
            .get_transaction_count(wallet.address(), None)
            .await?;
        
        info!("  ✅ {}: Connected ({} RPCs, nonce: {})", 
            config.name, multi_rpc.endpoints.len(), nonce);
        
        Ok(Self {
            config,
            multi_rpc,
            wallet,
            prices: DashMap::new(),
            nonce: RwLock::new(nonce.as_u64()),
        })
    }
    
    /// Get the current provider (with automatic failover)
    pub fn provider(&self) -> &Provider<Http> {
        self.multi_rpc.get_healthy_provider()
    }
    
    /// Legacy compatibility - direct provider access
    /// Prefer using provider() for failover support
    #[deprecated(note = "Use provider() for automatic failover")]
    pub fn get_provider(&self) -> &Provider<Http> {
        self.multi_rpc.get_provider()
    }
    
    /// Get and increment nonce atomically
    pub fn next_nonce(&self) -> u64 {
        let mut nonce = self.nonce.write();
        let current = *nonce;
        *nonce += 1;
        current
    }
    
    /// Reset nonce from chain
    pub async fn sync_nonce(&self) -> anyhow::Result<()> {
        let on_chain_nonce = self.provider()
            .get_transaction_count(self.wallet.address(), None)
            .await?;
        let mut nonce = self.nonce.write();
        *nonce = on_chain_nonce.as_u64();
        Ok(())
    }
    
    /// Get cached price or fetch
    pub fn get_price(&self, token: &Address, max_age_ms: u64) -> Option<f64> {
        if let Some(price_data) = self.prices.get(token) {
            if !price_data.is_stale(max_age_ms) {
                return Some(price_data.price_usd);
            }
        }
        None
    }
    
    /// Update price cache
    pub fn set_price(&self, token: Address, price: f64) {
        self.prices.insert(token, PriceData::new(price));
    }
    
    /// Record RPC success
    pub fn record_rpc_success(&self, latency_ms: u64) {
        self.multi_rpc.record_success(latency_ms);
    }
    
    /// Record RPC failure
    pub fn record_rpc_failure(&self) {
        self.multi_rpc.record_failure();
    }
}

/// Manages all chain connections and monitoring
pub struct ChainManager {
    pub config: Config,
    pub chains: DashMap<String, Arc<ChainState>>,
    pub stats: RwLock<Stats>,
    pub circuit_breaker: RwLock<CircuitBreaker>,
    pub execution_locks: DashMap<String, ExecutionLock>,
    shutdown: RwLock<bool>,
}

impl ChainManager {
    pub async fn new(config: &Config) -> anyhow::Result<Self> {
        let chains = DashMap::new();
        
        for chain_config in &config.chains {
            match ChainState::new(chain_config.clone(), &config.private_key).await {
                Ok(state) => {
                    chains.insert(chain_config.name.clone(), Arc::new(state));
                }
                Err(e) => {
                    error!("  ❌ {}: Failed to connect - {}", chain_config.name, e);
                }
            }
        }
        
        Ok(Self {
            config: config.clone(),
            chains,
            stats: RwLock::new(Stats::default()),
            circuit_breaker: RwLock::new(CircuitBreaker::default()),
            execution_locks: DashMap::new(),
            shutdown: RwLock::new(false),
        })
    }
    
    pub fn chain_count(&self) -> usize {
        self.chains.len()
    }
    
    pub fn get_chain(&self, name: &str) -> Option<Arc<ChainState>> {
        self.chains.get(name).map(|c| c.clone())
    }
    
    /// Start all monitoring loops
    pub async fn start_monitoring(&self) {
        info!("Starting monitoring loops...");
        
        // Background scan every 30 seconds
        let mut scan_interval = interval(Duration::from_secs(30));
        
        // Stats logging every 60 seconds
        let mut stats_interval = interval(Duration::from_secs(60));
        
        // Lock cleanup every 60 seconds  
        let mut cleanup_interval = interval(Duration::from_secs(60));
        
        // RPC health check every 30 seconds
        let mut health_interval = interval(Duration::from_secs(30));
        
        loop {
            if *self.shutdown.read() {
                break;
            }
            
            tokio::select! {
                _ = scan_interval.tick() => {
                    self.background_scan().await;
                }
                _ = stats_interval.tick() => {
                    self.log_stats();
                }
                _ = cleanup_interval.tick() => {
                    self.cleanup_stale_locks();
                }
                _ = health_interval.tick() => {
                    self.check_rpc_health().await;
                }
            }
        }
    }
    
    /// Check health of all RPC endpoints
    async fn check_rpc_health(&self) {
        for chain_ref in self.chains.iter() {
            let chain = chain_ref.value();
            chain.multi_rpc.health_check_all().await;
        }
    }
    
    /// Scan all chains for liquidatable positions
    async fn background_scan(&self) {
        let mut stats = self.stats.write();
        stats.checks += 1;
        drop(stats);
        
        for chain_ref in self.chains.iter() {
            let chain_name = chain_ref.key().clone();
            debug!("Scanning {}", chain_name);
        }
    }
    
    /// Try to acquire execution lock
    pub fn acquire_lock(&self, key: &str, chain: &str, protocol: Protocol) -> bool {
        // Check for expired lock
        if let Some(existing) = self.execution_locks.get(key) {
            if existing.is_expired(120_000) { // 2 minute timeout
                self.execution_locks.remove(key);
            } else {
                return false;
            }
        }
        
        self.execution_locks.insert(
            key.to_string(), 
            ExecutionLock::new(chain.to_string(), protocol)
        );
        true
    }
    
    /// Release execution lock
    pub fn release_lock(&self, key: &str) {
        self.execution_locks.remove(key);
    }
    
    /// Clean up stale locks
    fn cleanup_stale_locks(&self) {
        let mut to_remove = Vec::new();
        
        for lock_ref in self.execution_locks.iter() {
            if lock_ref.value().is_expired(120_000) {
                to_remove.push(lock_ref.key().clone());
            }
        }
        
        for key in to_remove {
            info!("🧹 Cleaning stale lock: {}", key);
            self.execution_locks.remove(&key);
        }
    }
    
    /// Log current stats
    fn log_stats(&self) {
        let stats = self.stats.read();
        let cb = self.circuit_breaker.read();
        let cb_status = if cb.is_open() { " [CIRCUIT OPEN]" } else { "" };
        
        info!(
            "Events: {} | Checks: {} | Attempted: {} | Success: {} | Failed: {} | Skipped: {} | Competitor: {}{}",
            stats.events,
            stats.checks,
            stats.attempted,
            stats.liquidations,
            stats.failed,
            stats.skipped_unprofitable,
            stats.competitor_beats,
            cb_status
        );
    }
    
    /// Check if circuit breaker is open
    pub fn is_circuit_open(&self) -> bool {
        self.circuit_breaker.read().is_open()
    }
    
    /// Record execution success
    pub fn record_success(&self) {
        self.circuit_breaker.write().record_success();
        self.stats.write().liquidations += 1;
    }
    
    /// Record execution failure
    pub fn record_failure(&self) {
        self.circuit_breaker.write().record_failure();
        self.stats.write().failed += 1;
    }
    
    /// Graceful shutdown
    pub async fn shutdown(&self) {
        *self.shutdown.write() = true;
        
        let stats = self.stats.read();
        info!("📊 Final Stats:");
        info!("   Liquidations: {}", stats.liquidations);
        info!("   Attempted: {}", stats.attempted);
        info!("   Failed: {}", stats.failed);
        info!("   Competitor Beats: {}", stats.competitor_beats);
    }
    
    /// Get health status including RPC status
    pub fn health_status(&self) -> serde_json::Value {
        let stats = self.stats.read();
        let cb = self.circuit_breaker.read();
        
        let chains_status: Vec<serde_json::Value> = self.chains.iter().map(|chain_ref| {
            let chain = chain_ref.value();
            serde_json::json!({
                "name": chain_ref.key(),
                "rpcs": chain.multi_rpc.status(),
            })
        }).collect();
        
        serde_json::json!({
            "status": if cb.is_open() { "degraded" } else { "healthy" },
            "chains": chains_status,
            "stats": {
                "events": stats.events,
                "checks": stats.checks,
                "liquidations": stats.liquidations,
                "attempted": stats.attempted,
                "failed": stats.failed,
                "skipped_unprofitable": stats.skipped_unprofitable,
                "competitor_beats": stats.competitor_beats,
            },
            "circuit_breaker": {
                "is_open": cb.is_open(),
                "consecutive_failures": cb.consecutive_failures,
            },
            "locks": self.execution_locks.len(),
        })
    }
}
