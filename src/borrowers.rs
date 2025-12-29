//! Borrower discovery from on-chain events.
//!
//! Discovers borrowers by scanning Borrow events from Aave pools.

use ethers::providers::{Provider, Http, Middleware};
use ethers::types::{Address, Filter, H256};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tokio::fs;
use tracing::{info, warn, debug, error};

/// Aave Borrow event signature
/// Borrow(address indexed reserve, address user, address indexed onBehalfOf, uint256 amount, uint8 interestRateMode, uint256 borrowRate, uint16 indexed referralCode)
pub const AAVE_BORROW_TOPIC: &str = "0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0";

/// V7.5 borrower entry format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BorrowerEntry {
    pub user: String,
}

/// Raw V7.5 format from disk
pub type RawBorrowerStore = HashMap<String, Vec<BorrowerEntry>>;

/// Normalized borrower storage
#[derive(Debug, Clone, Default)]
pub struct BorrowerStore {
    pub aave: HashMap<String, Vec<Address>>,
    pub last_scanned_block: HashMap<String, u64>,
}

impl BorrowerStore {
    /// Load from V7.5 format file
    pub async fn load(path: &Path) -> Self {
        let mut store = Self::default();
        
        let content = match fs::read_to_string(path).await {
            Ok(c) => c,
            Err(e) => {
                info!("No borrowers file found at {:?}: {}", path, e);
                return store;
            }
        };
        
        // Parse V7.5 format
        let raw: RawBorrowerStore = match serde_json::from_str(&content) {
            Ok(r) => r,
            Err(e) => {
                info!("Failed to parse borrowers.json: {}", e);
                return store;
            }
        };
        
        // Convert to normalized format
        for (chain, entries) in raw {
            let chain_lower = chain.to_lowercase();
            let chain_normalized = normalize_chain_name(&chain_lower);
            
            let addresses: Vec<Address> = entries
                .iter()
                .filter_map(|entry| entry.user.parse().ok())
                .collect();
            
            if !addresses.is_empty() {
                store.aave.insert(chain_normalized.to_string(), addresses);
            }
        }
        
        store
    }
    
    /// Save to disk
    pub async fn save(&self, path: &Path) -> anyhow::Result<()> {
        // Ensure data directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.ok();
        }
        
        // Convert to V7.5 format
        let mut raw: RawBorrowerStore = HashMap::new();
        for (chain, addresses) in &self.aave {
            let entries: Vec<BorrowerEntry> = addresses
                .iter()
                .map(|a| BorrowerEntry { user: format!("{:?}", a) })
                .collect();
            
            // Capitalize chain name for V7.5 compatibility
            let chain_cap = capitalize_chain_name(chain);
            raw.insert(chain_cap, entries);
        }
        
        let content = serde_json::to_string_pretty(&raw)?;
        fs::write(path, content).await?;
        
        Ok(())
    }
    
    /// Get borrowers for a chain
    pub fn get_aave_borrowers(&self, chain: &str) -> Vec<Address> {
        let chain_lower = chain.to_lowercase();
        self.aave.get(&chain_lower).cloned().unwrap_or_default()
    }
    
    /// Add discovered borrowers
    pub fn add_borrowers(&mut self, chain: &str, new_borrowers: Vec<Address>) -> usize {
        let chain_lower = chain.to_lowercase();
        let entry = self.aave.entry(chain_lower.clone()).or_default();
        
        let existing: HashSet<Address> = entry.iter().cloned().collect();
        let mut added = 0;
        
        for borrower in new_borrowers {
            if !existing.contains(&borrower) {
                entry.push(borrower);
                added += 1;
            }
        }
        
        added
    }
    
    /// Get total count
    pub fn total_count(&self) -> usize {
        self.aave.values().map(|v| v.len()).sum()
    }
}

/// Discover borrowers from Aave Borrow events
pub async fn discover_aave_borrowers(
    provider: &Provider<Http>,
    pool_address: Address,
    chain: &str,
    from_block: u64,
    to_block: Option<u64>,
) -> anyhow::Result<Vec<Address>> {
    let current_block = provider.get_block_number().await?.as_u64();
    let to_block = to_block.unwrap_or(current_block);
    
    info!("{}: Discovering borrowers from block {} to {}", chain, from_block, to_block);
    
    let borrow_topic: H256 = AAVE_BORROW_TOPIC.parse()?;
    
    let mut all_borrowers: HashSet<Address> = HashSet::new();
    
    // Query in chunks to avoid RPC limits
    let chunk_size = 10_000u64;
    let mut start = from_block;
    
    while start < to_block {
        let end = (start + chunk_size).min(to_block);
        
        let filter = Filter::new()
            .address(pool_address)
            .topic0(borrow_topic)
            .from_block(start)
            .to_block(end);
        
        match provider.get_logs(&filter).await {
            Ok(logs) => {
                for log in logs {
                    // onBehalfOf is topic[2] (the actual borrower)
                    if log.topics.len() > 2 {
                        let borrower = Address::from_slice(&log.topics[2].as_bytes()[12..]);
                        all_borrowers.insert(borrower);
                    }
                }
                debug!("{}: Scanned blocks {} to {} - found {} total borrowers", 
                    chain, start, end, all_borrowers.len());
            }
            Err(e) => {
                warn!("{}: Error fetching logs from {} to {}: {}", chain, start, end, e);
            }
        }
        
        start = end + 1;
        
        // Small delay to avoid rate limits
        if start < to_block {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
    
    let borrowers: Vec<Address> = all_borrowers.into_iter().collect();
    info!("{}: Discovered {} unique borrowers", chain, borrowers.len());
    
    Ok(borrowers)
}

/// Run initial borrower discovery for all chains
pub async fn discover_all_borrowers(
    providers: &HashMap<String, (Provider<Http>, Address)>,
    store: &mut BorrowerStore,
    blocks_back: u64,
) -> anyhow::Result<()> {
    info!("🔍 Starting borrower discovery ({} blocks back)...", blocks_back);
    
    for (chain, (provider, pool_address)) in providers {
        let current_block = match provider.get_block_number().await {
            Ok(b) => b.as_u64(),
            Err(e) => {
                error!("{}: Failed to get block number - {}", chain, e);
                continue;
            }
        };
        
        let from_block = current_block.saturating_sub(blocks_back);
        
        match discover_aave_borrowers(
            provider,
            *pool_address,
            chain,
            from_block,
            Some(current_block),
        ).await {
            Ok(borrowers) => {
                let added = store.add_borrowers(chain, borrowers);
                info!("{}: Added {} new borrowers (total: {})", 
                    chain, added, store.get_aave_borrowers(chain).len());
                store.last_scanned_block.insert(chain.clone(), current_block);
            }
            Err(e) => {
                error!("{}: Discovery failed - {}", chain, e);
            }
        }
    }
    
    info!("✅ Discovery complete: {} total borrowers", store.total_count());
    Ok(())
}

/// Incremental borrower discovery (since last scan)
pub async fn update_borrowers(
    provider: &Provider<Http>,
    pool_address: Address,
    chain: &str,
    store: &mut BorrowerStore,
) -> anyhow::Result<usize> {
    let current_block = provider.get_block_number().await?.as_u64();
    let last_block = store.last_scanned_block.get(chain).copied().unwrap_or(0);
    
    if last_block >= current_block {
        return Ok(0);
    }
    
    let borrowers = discover_aave_borrowers(
        provider,
        pool_address,
        chain,
        last_block + 1,
        Some(current_block),
    ).await?;
    
    let added = store.add_borrowers(chain, borrowers);
    store.last_scanned_block.insert(chain.to_string(), current_block);
    
    Ok(added)
}

fn normalize_chain_name(chain: &str) -> &str {
    match chain {
        "base" => "base",
        "polygon" => "polygon",
        "arbitrum" => "arbitrum",
        "avalanche" => "avalanche",
        "bnb" | "bsc" | "bnbchain" => "bnb",
        other => other,
    }
}

fn capitalize_chain_name(chain: &str) -> String {
    let mut chars = chain.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
