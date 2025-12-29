//! Compound V3 (Comet) protocol implementation.
//!
//! Compound V3 uses a single "Comet" contract per market (e.g., USDC market, WETH market).
//! Users supply collateral assets and borrow the base asset.

use ethers::prelude::*;
use ethers::providers::{Provider, Http};
use ethers::types::Address;
use std::sync::Arc;
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::types::{Position, Protocol};

// Comet (Compound V3) ABI
abigen!(
    IComet,
    r#"[
        function isLiquidatable(address account) external view returns (bool)
        function isBorrowCollateralized(address account) external view returns (bool)
        function borrowBalanceOf(address account) external view returns (uint256)
        function collateralBalanceOf(address account, address asset) external view returns (uint128)
        function getAssetInfo(uint8 i) external view returns (uint8 offset, address asset, address priceFeed, uint64 scale, uint64 borrowCollateralFactor, uint64 liquidateCollateralFactor, uint64 liquidationFactor, uint128 supplyCap)
        function numAssets() external view returns (uint8)
        function baseToken() external view returns (address)
        function baseTokenPriceFeed() external view returns (address)
        function getPrice(address priceFeed) external view returns (uint256)
        function absorb(address absorber, address[] calldata accounts) external
        function quoteCollateral(address asset, uint256 baseAmount) external view returns (uint256)
    ]"#
);

// Multicall3 ABI
abigen!(
    IMulticall3,
    r#"[
        function aggregate(address[] calldata targets, bytes[] calldata data) external payable returns (uint256 blockNumber, bytes[] memory returnData)
    ]"#
);

/// Compound V3 Comet addresses by chain
pub fn get_comet_addresses(chain: &str) -> Vec<(&'static str, &'static str, Address)> {
    match chain {
        "base" => vec![
            ("USDC", "USDbC", "0x9c4ec768c28520B50860ea7a15bd7213a9fF58bf".parse().unwrap()),
            ("WETH", "WETH", "0x46e6b214b524310239732D51387075E0e70970bf".parse().unwrap()),
        ],
        "polygon" => vec![
            ("USDC", "USDC.e", "0xF25212E676D1F7F89Cd72fFEe66158f541246445".parse().unwrap()),
        ],
        "arbitrum" => vec![
            ("USDC", "USDC.e", "0xA5EDBDD9646f8dFF606d7448e414884C7d905dCA".parse().unwrap()),
            ("USDC", "USDC", "0x9c4ec768c28520B50860ea7a15bd7213a9fF58bf".parse().unwrap()),
        ],
        _ => vec![],
    }
}

/// Collateral asset info
#[derive(Debug, Clone)]
pub struct CollateralAsset {
    pub asset: Address,
    pub price_feed: Address,
    pub scale: u64,
    pub liquidation_factor: u64,
}

/// User's Compound position
#[derive(Debug, Clone)]
pub struct CompoundPosition {
    pub user: Address,
    pub comet: Address,
    pub base_token: String,
    pub borrow_balance: U256,
    pub borrow_usd: f64,
    pub collaterals: Vec<(Address, U256, f64)>, // (asset, balance, usd_value)
    pub is_liquidatable: bool,
}

#[derive(Clone)]
pub struct CompoundProtocol {
    pub comet_address: Address,
    pub base_token_name: String,
    pub collateral_assets: Vec<CollateralAsset>,
}

impl CompoundProtocol {
    pub fn new(comet_address: Address, base_token_name: &str) -> Self {
        Self {
            comet_address,
            base_token_name: base_token_name.to_string(),
            collateral_assets: Vec::new(),
        }
    }
    
    /// Discover collateral assets for this Comet market
    pub async fn discover_assets(&mut self, provider: &Provider<Http>) -> anyhow::Result<()> {
        let comet = IComet::new(self.comet_address, Arc::new(provider.clone()));
        
        let num_assets = comet.num_assets().call().await?;
        info!("Compound {} market has {} collateral assets", self.base_token_name, num_assets);
        
        for i in 0..num_assets {
            match comet.get_asset_info(i).call().await {
                Ok(info) => {
                    self.collateral_assets.push(CollateralAsset {
                        asset: info.1,
                        price_feed: info.2,
                        scale: info.3,
                        liquidation_factor: info.6,
                    });
                    debug!("  Asset {}: {:?}", i, info.1);
                }
                Err(e) => {
                    warn!("Failed to get asset info {}: {}", i, e);
                }
            }
        }
        
        Ok(())
    }
    
    /// Check if a user is liquidatable
    pub async fn check_user(
        &self,
        provider: &Provider<Http>,
        user: Address,
        chain: &str,
    ) -> anyhow::Result<Option<Position>> {
        let comet = IComet::new(self.comet_address, Arc::new(provider.clone()));
        
        // Check if liquidatable
        let is_liquidatable = comet.is_liquidatable(user).call().await?;
        
        // Get borrow balance
        let borrow_balance = comet.borrow_balance_of(user).call().await?;
        
        // Skip if no borrow
        if borrow_balance.is_zero() {
            return Ok(None);
        }
        
        // Estimate USD value (assuming base token is a stablecoin with 6 decimals)
        let borrow_usd = borrow_balance.as_u128() as f64 / 1e6;
        
        // Skip small positions
        if borrow_usd < 100.0 {
            return Ok(None);
        }
        
        // Get total collateral value
        let mut total_collateral_usd = 0.0;
        for collateral in &self.collateral_assets {
            let balance = comet.collateral_balance_of(user, collateral.asset).call().await?;
            if balance > 0 {
                // Get price from Comet's price feed
                if let Ok(price) = comet.get_price(collateral.price_feed).call().await {
                    let price_usd = price.as_u128() as f64 / 1e8; // Chainlink uses 8 decimals
                    let balance_normalized = balance as f64 / (collateral.scale as f64);
                    total_collateral_usd += balance_normalized * price_usd;
                }
            }
        }
        
        // Calculate health factor equivalent
        let health_factor = if borrow_usd > 0.0 {
            total_collateral_usd / borrow_usd
        } else {
            999.0
        };
        
        Ok(Some(Position {
            user,
            chain: chain.to_string(),
            protocol: Protocol::Compound,
            collateral_usd: total_collateral_usd,
            debt_usd: borrow_usd,
            health_factor,
            liquidatable: is_liquidatable,
        }))
    }
    
    /// Batch check multiple users using Multicall3
    pub async fn batch_check_users(
        &self,
        provider: &Provider<Http>,
        users: &[Address],
        chain: &str,
    ) -> anyhow::Result<Vec<Position>> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        
        let multicall_addr: Address = "0xcA11bde05977b3631167028862bE2a173976CA11".parse()?;
        let mut all_positions = Vec::new();
        
        // Process in batches of 100
        for batch in users.chunks(100) {
            match self.multicall_check_users(provider, multicall_addr, batch, chain).await {
                Ok(positions) => {
                    all_positions.extend(positions);
                }
                Err(e) => {
                    // Fallback to sequential
                    debug!("Compound multicall failed, falling back: {}", e);
                    for user in batch {
                        if let Ok(Some(pos)) = self.check_user(provider, *user, chain).await {
                            all_positions.push(pos);
                        }
                    }
                }
            }
        }
        
        debug!("Compound: Checked {} users, found {} positions", users.len(), all_positions.len());
        Ok(all_positions)
    }
    
    /// Use Multicall3 to batch check isLiquidatable
    async fn multicall_check_users(
        &self,
        provider: &Provider<Http>,
        multicall_addr: Address,
        users: &[Address],
        chain: &str,
    ) -> anyhow::Result<Vec<Position>> {
        use ethers::abi::{Function, Param, ParamType, Token, StateMutability};
        
        // isLiquidatable function
        let is_liq_fn = Function {
            name: "isLiquidatable".to_string(),
            inputs: vec![Param {
                name: "account".to_string(),
                kind: ParamType::Address,
                internal_type: None,
            }],
            outputs: vec![Param {
                name: "".to_string(),
                kind: ParamType::Bool,
                internal_type: None,
            }],
            constant: None,
            state_mutability: StateMutability::View,
        };
        
        // Build calls
        let mut targets = Vec::with_capacity(users.len());
        let mut call_data = Vec::with_capacity(users.len());
        
        for user in users {
            let encoded = is_liq_fn.encode_input(&[Token::Address(*user)])?;
            targets.push(self.comet_address);
            call_data.push(ethers::types::Bytes::from(encoded));
        }
        
        // Call multicall
        let multicall = IMulticall3::new(multicall_addr, Arc::new(provider.clone()));
        let (_, results) = multicall.aggregate(targets, call_data).call().await?;
        
        let mut positions = Vec::new();
        
        // Only fetch full details for liquidatable users
        for (i, result_bytes) in results.iter().enumerate() {
            if result_bytes.len() < 32 {
                continue;
            }
            
            let decoded = is_liq_fn.decode_output(result_bytes)?;
            let is_liquidatable = match decoded.get(0) {
                Some(Token::Bool(b)) => *b,
                _ => false,
            };
            
            if is_liquidatable {
                // Fetch full details for liquidatable user
                if let Ok(Some(pos)) = self.check_user(provider, users[i], chain).await {
                    positions.push(pos);
                }
            }
        }
        
        Ok(positions)
    }
    
    /// Get liquidation details for a user
    pub async fn get_liquidation_details(
        &self,
        provider: &Provider<Http>,
        user: Address,
    ) -> anyhow::Result<Option<CompoundPosition>> {
        let comet = IComet::new(self.comet_address, Arc::new(provider.clone()));
        
        let is_liquidatable = comet.is_liquidatable(user).call().await?;
        if !is_liquidatable {
            return Ok(None);
        }
        
        let borrow_balance = comet.borrow_balance_of(user).call().await?;
        let borrow_usd = borrow_balance.as_u128() as f64 / 1e6;
        
        let mut collaterals = Vec::new();
        for collateral in &self.collateral_assets {
            let balance = comet.collateral_balance_of(user, collateral.asset).call().await?;
            if balance > 0 {
                let price = comet.get_price(collateral.price_feed).call().await.unwrap_or(U256::zero());
                let price_usd = price.as_u128() as f64 / 1e8;
                let balance_normalized = balance as f64 / (collateral.scale as f64);
                let usd_value = balance_normalized * price_usd;
                collaterals.push((collateral.asset, U256::from(balance), usd_value));
            }
        }
        
        Ok(Some(CompoundPosition {
            user,
            comet: self.comet_address,
            base_token: self.base_token_name.clone(),
            borrow_balance,
            borrow_usd,
            collaterals,
            is_liquidatable,
        }))
    }
}

/// Discover Compound borrowers from Withdraw events (indicates active borrowers)
pub async fn discover_compound_borrowers(
    provider: &Provider<Http>,
    comet_address: Address,
    chain: &str,
    from_block: u64,
    to_block: Option<u64>,
) -> anyhow::Result<Vec<Address>> {
    use std::collections::HashSet;
    
    let current_block = provider.get_block_number().await?.as_u64();
    let to_block = to_block.unwrap_or(current_block);
    
    // Withdraw event topic (when users borrow, they withdraw the base token)
    // Withdraw(address indexed src, address indexed to, uint256 amount)
    let withdraw_topic: H256 = "0x9b1bfa7fa9ee420a16e124f794c35ac9f90472acc99140eb2f6447c714cad8eb".parse()?;
    
    let mut all_borrowers: HashSet<Address> = HashSet::new();
    
    let chunk_size = 10_000u64;
    let mut start = from_block;
    
    while start < to_block {
        let end = (start + chunk_size).min(to_block);
        
        let filter = Filter::new()
            .address(comet_address)
            .topic0(withdraw_topic)
            .from_block(start)
            .to_block(end);
        
        match provider.get_logs(&filter).await {
            Ok(logs) => {
                for log in logs {
                    // src is topic[1] (the borrower)
                    if log.topics.len() > 1 {
                        let borrower = Address::from_slice(&log.topics[1].as_bytes()[12..]);
                        all_borrowers.insert(borrower);
                    }
                }
            }
            Err(e) => {
                warn!("{}: Error fetching Compound logs: {}", chain, e);
            }
        }
        
        start = end + 1;
        
        if start < to_block {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    }
    
    let borrowers: Vec<Address> = all_borrowers.into_iter().collect();
    info!("{}: Discovered {} Compound borrowers", chain, borrowers.len());
    
    Ok(borrowers)
}
