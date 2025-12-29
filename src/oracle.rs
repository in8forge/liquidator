//! WebSocket price feed subscriptions with multi-provider redundancy.
//!
//! During Oct 10-style crashes, WS providers drop connections under load.
//! This module supports:
//! - Multiple WS connections per chain
//! - Automatic reconnection with exponential backoff
//! - Cross-validation of prices between providers
//! - Health monitoring per connection

use ethers::types::Address;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::chains::ChainManager;

/// Maximum price deviation allowed between providers (5%)
const MAX_PRICE_DEVIATION: f64 = 0.05;

/// Health check interval for WS connections
const HEALTH_CHECK_INTERVAL_SECS: u64 = 30;

/// Chainlink price feed addresses by chain and token
pub fn get_chainlink_feeds() -> HashMap<String, HashMap<Address, Address>> {
    let mut feeds: HashMap<String, HashMap<Address, Address>> = HashMap::new();
    
    // Base
    let mut base_feeds = HashMap::new();
    base_feeds.insert(
        "0x4200000000000000000000000000000000000006".parse().unwrap(), // WETH
        "0x71041dddad3595F9CEd3DcCFBe3D1F4b0a16Bb70".parse().unwrap(),
    );
    base_feeds.insert(
        "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse().unwrap(), // USDC
        "0x7e860098F58bBFC8648a4311b374B1D669a2bc6B".parse().unwrap(),
    );
    feeds.insert("base".to_string(), base_feeds);
    
    // Polygon
    let mut polygon_feeds = HashMap::new();
    polygon_feeds.insert(
        "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619".parse().unwrap(), // WETH
        "0xF9680D99D6C9589e2a93a78A04A279e509205945".parse().unwrap(),
    );
    polygon_feeds.insert(
        "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270".parse().unwrap(), // WMATIC
        "0xAB594600376Ec9fD91F8e885dADF0CE036862dE0".parse().unwrap(),
    );
    feeds.insert("polygon".to_string(), polygon_feeds);
    
    // Arbitrum
    let mut arb_feeds = HashMap::new();
    arb_feeds.insert(
        "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1".parse().unwrap(), // WETH
        "0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612".parse().unwrap(),
    );
    feeds.insert("arbitrum".to_string(), arb_feeds);
    
    // Avalanche
    let mut avax_feeds = HashMap::new();
    avax_feeds.insert(
        "0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7".parse().unwrap(), // WAVAX
        "0x0A77230d17318075983913bC2145DB16C7366156".parse().unwrap(),
    );
    feeds.insert("avalanche".to_string(), avax_feeds);
    
    // BNB Chain
    let mut bnb_feeds = HashMap::new();
    bnb_feeds.insert(
        "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c".parse().unwrap(), // WBNB
        "0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE".parse().unwrap(),
    );
    feeds.insert("bnb".to_string(), bnb_feeds);
    
    feeds
}

/// Price update event
#[derive(Debug, Clone)]
pub struct PriceUpdate {
    pub chain: String,
    pub token: Address,
    pub price: f64,
    pub timestamp: u64,
    pub provider_index: usize,
}

/// WebSocket connection health status
#[derive(Debug)]
pub struct WsConnectionHealth {
    pub url: String,
    pub is_connected: AtomicBool,
    pub last_message: RwLock<Instant>,
    pub messages_received: AtomicU64,
    pub reconnect_count: AtomicU64,
}

impl WsConnectionHealth {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            is_connected: AtomicBool::new(false),
            last_message: RwLock::new(Instant::now()),
            messages_received: AtomicU64::new(0),
            reconnect_count: AtomicU64::new(0),
        }
    }
    
    fn mark_connected(&self) {
        self.is_connected.store(true, Ordering::Relaxed);
    }
    
    fn mark_disconnected(&self) {
        self.is_connected.store(false, Ordering::Relaxed);
        self.reconnect_count.fetch_add(1, Ordering::Relaxed);
    }
    
    fn record_message(&self) {
        *self.last_message.write() = Instant::now();
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }
    
    fn seconds_since_last_message(&self) -> u64 {
        self.last_message.read().elapsed().as_secs()
    }
    
    fn is_healthy(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed) 
            && self.seconds_since_last_message() < 120
    }
}

/// WebSocket response structures
#[derive(Deserialize, Debug)]
struct WsResponse {
    #[serde(default)]
    params: Option<WsParams>,
}

#[derive(Deserialize, Debug)]
struct WsParams {
    result: WsLog,
}

#[derive(Deserialize, Debug)]
struct WsLog {
    #[serde(default)]
    address: String,
    #[serde(default)]
    data: String,
}

/// Oracle manager with multi-provider redundancy
pub struct OracleManager {
    chain_manager: Arc<ChainManager>,
    price_tx: mpsc::Sender<PriceUpdate>,
    feeds: HashMap<String, HashMap<Address, Address>>,
    /// Health tracking for all WS connections: chain -> provider_index -> health
    connection_health: Arc<RwLock<HashMap<String, Vec<Arc<WsConnectionHealth>>>>>,
    /// Last known prices per token for cross-validation
    last_prices: Arc<RwLock<HashMap<(String, Address), Vec<(f64, Instant, usize)>>>>,
}

impl OracleManager {
    pub fn new(
        chain_manager: Arc<ChainManager>,
        price_tx: mpsc::Sender<PriceUpdate>,
    ) -> Self {
        Self {
            chain_manager,
            price_tx,
            feeds: get_chainlink_feeds(),
            connection_health: Arc::new(RwLock::new(HashMap::new())),
            last_prices: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    
    /// Start all WebSocket subscriptions with redundancy
    pub async fn start(&self) {
        info!("📡 Starting oracle subscriptions with redundancy...");
        
        for (chain, feeds) in &self.feeds {
            let ws_urls = self.get_ws_urls(chain);
            
            if ws_urls.is_empty() {
                warn!("  ⚠️ {}: No WebSocket URLs configured", chain);
                continue;
            }
            
            // Initialize health tracking for this chain
            {
                let mut health_map = self.connection_health.write();
                let chain_health: Vec<Arc<WsConnectionHealth>> = ws_urls
                    .iter()
                    .map(|url| Arc::new(WsConnectionHealth::new(url)))
                    .collect();
                health_map.insert(chain.clone(), chain_health);
            }
            
            // Start a connection for each WS URL
            for (provider_idx, ws_url) in ws_urls.iter().enumerate() {
                let chain_clone = chain.clone();
                let ws_url_clone = ws_url.clone();
                let feeds_clone = feeds.clone();
                let price_tx = self.price_tx.clone();
                let cm = self.chain_manager.clone();
                let health = self.connection_health.clone();
                let last_prices = self.last_prices.clone();
                
                tokio::spawn(async move {
                    Self::run_provider_subscription(
                        chain_clone,
                        ws_url_clone,
                        provider_idx,
                        feeds_clone,
                        price_tx,
                        cm,
                        health,
                        last_prices,
                    ).await;
                });
            }
            
            info!("  ✅ {}: {} feeds on {} providers", chain, feeds.len(), ws_urls.len());
        }
        
        // Start health monitoring
        let health_clone = self.connection_health.clone();
        tokio::spawn(async move {
            Self::health_monitor(health_clone).await;
        });
    }
    
    /// Get all WS URLs for a chain (comma-separated in config)
    fn get_ws_urls(&self, chain: &str) -> Vec<String> {
        self.chain_manager
            .get_chain(chain)
            .and_then(|c| c.config.ws_url.clone())
            .map(|urls| {
                urls.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
    
    /// Run WebSocket subscription for a single provider
    async fn run_provider_subscription(
        chain: String,
        ws_url: String,
        provider_idx: usize,
        feeds: HashMap<Address, Address>,
        price_tx: mpsc::Sender<PriceUpdate>,
        chain_manager: Arc<ChainManager>,
        health_map: Arc<RwLock<HashMap<String, Vec<Arc<WsConnectionHealth>>>>>,
        last_prices: Arc<RwLock<HashMap<(String, Address), Vec<(f64, Instant, usize)>>>>,
    ) {
        let mut retry_delay = Duration::from_secs(1);
        let max_retry_delay = Duration::from_secs(60);
        
        // Get health tracker for this connection
        let health = {
            let map = health_map.read();
            map.get(&chain)
                .and_then(|v| v.get(provider_idx))
                .cloned()
        };
        
        loop {
            let result = Self::connect_and_subscribe(
                &chain,
                &ws_url,
                provider_idx,
                &feeds,
                &price_tx,
                &chain_manager,
                health.as_ref(),
                &last_prices,
            ).await;
            
            if let Some(ref h) = health {
                h.mark_disconnected();
            }
            
            match result {
                Ok(_) => {
                    retry_delay = Duration::from_secs(1);
                }
                Err(e) => {
                    error!("{} [{}]: WebSocket error - {}", chain, provider_idx, e);
                }
            }
            
            debug!("{} [{}]: Reconnecting in {:?}...", chain, provider_idx, retry_delay);
            tokio::time::sleep(retry_delay).await;
            
            retry_delay = (retry_delay * 2).min(max_retry_delay);
        }
    }
    
    async fn connect_and_subscribe(
        chain: &str,
        ws_url: &str,
        provider_idx: usize,
        feeds: &HashMap<Address, Address>,
        price_tx: &mpsc::Sender<PriceUpdate>,
        chain_manager: &Arc<ChainManager>,
        health: Option<&Arc<WsConnectionHealth>>,
        last_prices: &Arc<RwLock<HashMap<(String, Address), Vec<(f64, Instant, usize)>>>>,
    ) -> anyhow::Result<()> {
        let connect_result = timeout(
            Duration::from_secs(10),
            connect_async(ws_url),
        ).await??;
        
        let (mut ws_stream, _) = connect_result;
        
        if let Some(h) = health {
            h.mark_connected();
        }
        
        debug!("{} [{}]: Connected to {}", chain, provider_idx, ws_url);
        
        // Build reverse lookup: feed_address -> token_address
        let feed_to_token: HashMap<String, Address> = feeds
            .iter()
            .map(|(token, feed)| (format!("{:?}", feed).to_lowercase(), *token))
            .collect();
        
        // AnswerUpdated event topic
        let answer_updated_topic = "0x0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f";
        
        // Subscribe to all feeds
        for (_, feed_address) in feeds {
            let subscribe_msg = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_subscribe",
                "params": [
                    "logs",
                    {
                        "address": format!("{:?}", feed_address),
                        "topics": [answer_updated_topic]
                    }
                ]
            });
            
            ws_stream.send(Message::Text(subscribe_msg.to_string())).await?;
        }
        
        while let Some(msg_result) = ws_stream.next().await {
            let msg = match msg_result {
                Ok(m) => m,
                Err(e) => {
                    error!("{} [{}]: Receive error - {}", chain, provider_idx, e);
                    break;
                }
            };
            
            match msg {
                Message::Text(text) => {
                    if let Ok(response) = serde_json::from_str::<WsResponse>(&text) {
                        if let Some(params) = response.params {
                            let feed_addr = params.result.address.to_lowercase();
                            
                            if let Some(token) = feed_to_token.get(&feed_addr) {
                                if let Some(price) = parse_chainlink_price(&params.result.data) {
                                    // Record health
                                    if let Some(h) = health {
                                        h.record_message();
                                    }
                                    
                                    // Cross-validate price
                                    let is_valid = Self::validate_price(
                                        chain,
                                        *token,
                                        price,
                                        provider_idx,
                                        last_prices,
                                    );
                                    
                                    if !is_valid {
                                        warn!(
                                            "{} [{}]: Price ${:.2} for {:?} deviates >{}% from other providers",
                                            chain, provider_idx, price, token, (MAX_PRICE_DEVIATION * 100.0) as u32
                                        );
                                        continue;
                                    }
                                    
                                    // Update chain state
                                    if let Some(chain_state) = chain_manager.get_chain(chain) {
                                        chain_state.set_price(*token, price);
                                    }
                                    
                                    chain_manager.stats.write().events += 1;
                                    
                                    let update = PriceUpdate {
                                        chain: chain.to_string(),
                                        token: *token,
                                        price,
                                        timestamp: std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap()
                                            .as_secs(),
                                        provider_index: provider_idx,
                                    };
                                    
                                    if price_tx.send(update).await.is_err() {
                                        error!("{} [{}]: Price channel closed", chain, provider_idx);
                                        break;
                                    }
                                    
                                    debug!(
                                        "{} [{}]: {:?} = ${:.2}",
                                        chain, provider_idx, token, price
                                    );
                                }
                            }
                        }
                    }
                }
                Message::Ping(data) => {
                    if ws_stream.send(Message::Pong(data)).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => {
                    info!("{} [{}]: Closed by server", chain, provider_idx);
                    break;
                }
                _ => {}
            }
        }
        
        Ok(())
    }
    
    /// Validate price against other providers (returns true if valid)
    fn validate_price(
        chain: &str,
        token: Address,
        price: f64,
        provider_idx: usize,
        last_prices: &Arc<RwLock<HashMap<(String, Address), Vec<(f64, Instant, usize)>>>>,
    ) -> bool {
        let key = (chain.to_string(), token);
        let now = Instant::now();
        let max_age = Duration::from_secs(60);
        
        let mut prices = last_prices.write();
        
        // Get or create price list for this token
        let price_list = prices.entry(key).or_insert_with(Vec::new);
        
        // Remove old prices and prices from this provider
        price_list.retain(|(_, ts, idx)| {
            now.duration_since(*ts) < max_age && *idx != provider_idx
        });
        
        // Check deviation from other providers
        let mut is_valid = true;
        for (other_price, _, _) in price_list.iter() {
            let deviation = ((price - other_price) / other_price).abs();
            if deviation > MAX_PRICE_DEVIATION {
                is_valid = false;
                break;
            }
        }
        
        // Store this price
        price_list.push((price, now, provider_idx));
        
        // If this is the first/only price, accept it
        if price_list.len() <= 1 {
            return true;
        }
        
        is_valid
    }
    
    /// Monitor health of all connections
    async fn health_monitor(health_map: Arc<RwLock<HashMap<String, Vec<Arc<WsConnectionHealth>>>>>) {
        let mut interval = tokio::time::interval(Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS));
        
        loop {
            interval.tick().await;
            
            let map = health_map.read();
            
            for (chain, providers) in map.iter() {
                let healthy_count = providers.iter().filter(|p| p.is_healthy()).count();
                let total_count = providers.len();
                
                if healthy_count == 0 {
                    error!(
                        "🚨 {}: ALL {} WS providers unhealthy!",
                        chain, total_count
                    );
                } else if healthy_count < total_count {
                    warn!(
                        "⚠️ {}: {}/{} WS providers healthy",
                        chain, healthy_count, total_count
                    );
                } else {
                    debug!("{}: {}/{} WS providers healthy", chain, healthy_count, total_count);
                }
                
                // Log details for debugging
                for (idx, provider) in providers.iter().enumerate() {
                    if !provider.is_healthy() {
                        debug!(
                            "  [{}] {} - connected: {}, last msg: {}s ago, reconnects: {}",
                            idx,
                            provider.url,
                            provider.is_connected.load(Ordering::Relaxed),
                            provider.seconds_since_last_message(),
                            provider.reconnect_count.load(Ordering::Relaxed),
                        );
                    }
                }
            }
        }
    }
    
    /// Get health status for all connections
    pub fn health_status(&self) -> serde_json::Value {
        let map = self.connection_health.read();
        
        let chains: Vec<serde_json::Value> = map.iter().map(|(chain, providers)| {
            let provider_status: Vec<serde_json::Value> = providers.iter().enumerate().map(|(idx, p)| {
                serde_json::json!({
                    "index": idx,
                    "url": p.url,
                    "connected": p.is_connected.load(Ordering::Relaxed),
                    "healthy": p.is_healthy(),
                    "messages": p.messages_received.load(Ordering::Relaxed),
                    "reconnects": p.reconnect_count.load(Ordering::Relaxed),
                    "last_message_secs": p.seconds_since_last_message(),
                })
            }).collect();
            
            let healthy_count = providers.iter().filter(|p| p.is_healthy()).count();
            
            serde_json::json!({
                "chain": chain,
                "healthy_providers": healthy_count,
                "total_providers": providers.len(),
                "providers": provider_status,
            })
        }).collect();
        
        serde_json::json!({
            "websockets": chains,
        })
    }
}

/// Parse Chainlink price from event data
fn parse_chainlink_price(data: &str) -> Option<f64> {
    let data = data.strip_prefix("0x").unwrap_or(data);
    
    if data.len() < 64 {
        return None;
    }
    
    let price_hex = &data[0..64];
    let price_bytes = hex::decode(price_hex).ok()?;
    
    if price_bytes.len() != 32 {
        return None;
    }
    
    // Take last 16 bytes as u128
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&price_bytes[16..32]);
    let price_raw = u128::from_be_bytes(arr);
    
    let price = price_raw as f64 / 1e8;
    
    if price > 0.0 && price < 1_000_000.0 {
        Some(price)
    } else {
        None
    }
}
