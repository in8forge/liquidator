//! Aave V3 protocol implementation with full asset discovery.

use ethers::prelude::*;
use ethers::providers::{Provider, Http, Middleware};
use ethers::types::{Address, U256, Bytes};
use std::sync::Arc;
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::types::{Position, Protocol, Asset};

// Aave Pool ABI
abigen!(
    IAavePool,
    r#"[
        function getUserAccountData(address user) external view returns (uint256 totalCollateralBase, uint256 totalDebtBase, uint256 availableBorrowsBase, uint256 currentLiquidationThreshold, uint256 ltv, uint256 healthFactor)
        function getReservesList() external view returns (address[] memory)
    ]"#
);

// Aave Data Provider ABI
abigen!(
    IAaveDataProvider,
    r#"[
        function getAllReservesTokens() external view returns (address[] memory)
        function getReserveTokensAddresses(address asset) external view returns (address aTokenAddress, address stableDebtTokenAddress, address variableDebtTokenAddress)
        function getReserveConfigurationData(address asset) external view returns (uint256 decimals, uint256 ltv, uint256 liquidationThreshold, uint256 liquidationBonus, uint256 reserveFactor, bool usageAsCollateralEnabled, bool borrowingEnabled, bool stableBorrowRateEnabled, bool isActive, bool isFrozen)
        function getUserReserveData(address asset, address user) external view returns (uint256 currentATokenBalance, uint256 currentStableDebt, uint256 currentVariableDebt, uint256 principalStableDebt, uint256 scaledVariableDebt, uint256 stableBorrowRate, uint256 liquidityRate, uint40 stableRateLastUpdated, bool usageAsCollateralEnabled)
    ]"#
);

// ERC20 ABI for balance checks and metadata
abigen!(
    IERC20,
    r#"[
        function balanceOf(address account) external view returns (uint256)
        function decimals() external view returns (uint8)
        function symbol() external view returns (string memory)
    ]"#
);

// Multicall3 ABI - simple version without tuples
abigen!(
    IMulticall3,
    r#"[
        function aggregate(address[] calldata targets, bytes[] calldata data) external payable returns (uint256 blockNumber, bytes[] memory returnData)
    ]"#
);

/// Multicall3 address (same on all chains)
pub const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// User's position details for a specific asset
#[derive(Debug, Clone)]
pub struct UserAssetPosition {
    pub asset: Address,
    pub symbol: String,
    pub decimals: u8,
    pub a_token: Address,
    pub debt_token: Address,
    pub collateral_balance: U256,
    pub collateral_usd: f64,
    pub debt_balance: U256,
    pub debt_usd: f64,
}

/// Full liquidation opportunity with all details needed to execute
#[derive(Debug, Clone)]
pub struct LiquidationOpportunity {
    pub user: Address,
    pub chain: String,
    pub health_factor: f64,
    pub total_collateral_usd: f64,
    pub total_debt_usd: f64,
    pub best_collateral: UserAssetPosition,
    pub best_debt: UserAssetPosition,
    pub liquidation_bonus: u64,
}

#[derive(Debug, Clone)]
pub struct AssetConfig {
    pub decimals: u8,
    pub liquidation_bonus: u64,
    pub a_token: Address,
    pub debt_token: Address,
}

#[derive(Clone)]
pub struct AaveProtocol {
    pub pool_address: Address,
    pub data_provider_address: Address,
    pub assets: Vec<Asset>,
    pub asset_configs: HashMap<Address, AssetConfig>,
}

impl AaveProtocol {
    pub fn new(pool_address: &str, data_provider: &str) -> anyhow::Result<Self> {
        Ok(Self {
            pool_address: pool_address.parse()?,
            data_provider_address: data_provider.parse()?,
            assets: Vec::new(),
            asset_configs: HashMap::new(),
        })
    }
    
    /// Discover all reserve assets and their configurations
    pub async fn discover_assets(&mut self, provider: &Provider<Http>) -> anyhow::Result<()> {
        let pool = IAavePool::new(self.pool_address, Arc::new(provider.clone()));
        
        // Get all reserve addresses
        let reserves = pool.get_reserves_list().call().await?;
        
        info!("Discovering {} Aave assets...", reserves.len());
        
        let data_provider = IAaveDataProvider::new(
            self.data_provider_address,
            Arc::new(provider.clone()),
        );
        
        for token_address in reserves {
            // Get token addresses (aToken, debtToken)
            let token_addrs = match data_provider
                .get_reserve_tokens_addresses(token_address)
                .call()
                .await {
                    Ok(t) => t,
                    Err(_) => continue,
                };
            
            // Get configuration
            let config = match data_provider
                .get_reserve_configuration_data(token_address)
                .call()
                .await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
            
            // Get symbol
            let erc20 = IERC20::new(token_address, Arc::new(provider.clone()));
            let symbol = erc20.symbol().call().await.unwrap_or_else(|_| "???".to_string());
            
            let decimals = config.0.as_u64() as u8;
            let liquidation_bonus = config.3.as_u64();
            
            self.assets.push(Asset {
                symbol: symbol.clone(),
                token: token_address,
                a_token: token_addrs.0,
                debt_token: token_addrs.2, // Variable debt token
                decimals,
            });
            
            self.asset_configs.insert(token_address, AssetConfig {
                decimals,
                liquidation_bonus,
                a_token: token_addrs.0,
                debt_token: token_addrs.2,
            });
            
            debug!("  {} ({} decimals, {}% bonus)", symbol, decimals, liquidation_bonus / 100);
        }
        
        info!("Discovered {} assets", self.assets.len());
        Ok(())
    }
    
    /// Check user account data
    pub async fn get_user_data(
        &self,
        provider: &Provider<Http>,
        user: Address,
    ) -> anyhow::Result<(f64, f64, f64)> {
        let pool = IAavePool::new(self.pool_address, Arc::new(provider.clone()));
        let data = pool.get_user_account_data(user).call().await?;
        
        let collateral = u256_to_f64_safe(data.0, 8);
        let debt = u256_to_f64_safe(data.1, 8);
        let health_factor = u256_to_f64_safe(data.5, 18);
        
        Ok((collateral, debt, health_factor))
    }
    
    /// Batch check multiple users using Multicall3 for efficiency
    /// Checks up to 100 users per RPC call instead of 1 user per call
    pub async fn batch_check_users(
        &self,
        provider: &Provider<Http>,
        users: &[Address],
        chain_name: &str,
    ) -> anyhow::Result<Vec<Position>> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        
        let multicall_addr: Address = MULTICALL3.parse()?;
        let mut all_positions = Vec::new();
        
        // Process in batches of 100
        for batch in users.chunks(100) {
            match self.multicall_check_users(provider, multicall_addr, batch, chain_name).await {
                Ok(positions) => {
                    all_positions.extend(positions);
                }
                Err(e) => {
                    // Fallback to sequential on multicall failure
                    debug!("Multicall failed, falling back to sequential: {}", e);
                    for user in batch {
                        if let Ok((collateral, debt, health_factor)) = self.get_user_data(provider, *user).await {
                            if debt >= 100.0 {
                                let liquidatable = health_factor > 0.0 && health_factor < 1.0;
                                all_positions.push(Position {
                                    user: *user,
                                    chain: chain_name.to_string(),
                                    protocol: Protocol::Aave,
                                    collateral_usd: collateral,
                                    debt_usd: debt,
                                    health_factor,
                                    liquidatable,
                                });
                            }
                        }
                    }
                }
            }
        }
        
        debug!("Checked {} users, found {} valid positions", users.len(), all_positions.len());
        Ok(all_positions)
    }
    
    /// Use Multicall3 to batch check users
    async fn multicall_check_users(
        &self,
        provider: &Provider<Http>,
        multicall_addr: Address,
        users: &[Address],
        chain_name: &str,
    ) -> anyhow::Result<Vec<Position>> {
        use ethers::abi::{Function, Param, ParamType, Token, StateMutability};
        
        // Build getUserAccountData function signature
        let get_user_data_fn = Function {
            name: "getUserAccountData".to_string(),
            inputs: vec![Param {
                name: "user".to_string(),
                kind: ParamType::Address,
                internal_type: None,
            }],
            outputs: vec![
                Param { name: "totalCollateralBase".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "totalDebtBase".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "availableBorrowsBase".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "currentLiquidationThreshold".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "ltv".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "healthFactor".to_string(), kind: ParamType::Uint(256), internal_type: None },
            ],
            constant: None,
            state_mutability: StateMutability::View,
        };
        
        // Build call data for each user
        let mut targets = Vec::with_capacity(users.len());
        let mut call_data = Vec::with_capacity(users.len());
        
        for user in users {
            let encoded = get_user_data_fn.encode_input(&[Token::Address(*user)])?;
            targets.push(self.pool_address);
            call_data.push(ethers::types::Bytes::from(encoded));
        }
        
        // Call multicall
        let multicall = IMulticall3::new(multicall_addr, Arc::new(provider.clone()));
        let (_, results) = multicall.aggregate(targets, call_data).call().await?;
        
        let mut positions = Vec::new();
        
        for (i, result_bytes) in results.iter().enumerate() {
            if result_bytes.len() < 192 {
                continue;
            }
            
            // Decode the 6 uint256 values
            let decoded = get_user_data_fn.decode_output(result_bytes)?;
            
            if decoded.len() < 6 {
                continue;
            }
            
            let collateral = match &decoded[0] {
                Token::Uint(v) => u256_to_f64_safe(*v, 8),
                _ => continue,
            };
            
            let debt = match &decoded[1] {
                Token::Uint(v) => u256_to_f64_safe(*v, 8),
                _ => continue,
            };
            
            let health_factor = match &decoded[5] {
                Token::Uint(v) => u256_to_f64_safe(*v, 18),
                _ => continue,
            };
            
            if debt < 100.0 {
                continue;
            }
            
            let liquidatable = health_factor > 0.0 && health_factor < 1.0;
            
            positions.push(Position {
                user: users[i],
                chain: chain_name.to_string(),
                protocol: Protocol::Aave,
                collateral_usd: collateral,
                debt_usd: debt,
                health_factor,
                liquidatable,
            });
        }
        
        Ok(positions)
    }
    
    /// Get user's full position details (collateral and debt per asset)
    pub async fn get_user_positions(
        &self,
        provider: &Provider<Http>,
        user: Address,
        prices: &HashMap<Address, f64>,
    ) -> anyhow::Result<Vec<UserAssetPosition>> {
        if self.assets.is_empty() {
            warn!("No assets discovered - call discover_assets first");
            return Ok(Vec::new());
        }
        
        let data_provider = IAaveDataProvider::new(
            self.data_provider_address,
            Arc::new(provider.clone()),
        );
        
        let mut positions = Vec::new();
        
        for asset in &self.assets {
            let user_data = match data_provider
                .get_user_reserve_data(asset.token, user)
                .call()
                .await {
                    Ok(d) => d,
                    Err(_) => continue,
                };
            
            let collateral_balance = user_data.0; // currentATokenBalance
            let debt_balance = user_data.2; // currentVariableDebt
            
            if collateral_balance.is_zero() && debt_balance.is_zero() {
                continue;
            }
            
            let price = prices.get(&asset.token).copied().unwrap_or(1.0);
            let decimals_factor = 10_f64.powi(asset.decimals as i32);
            
            let collateral_usd = u256_to_f64_safe(collateral_balance, 0) / decimals_factor * price;
            let debt_usd = u256_to_f64_safe(debt_balance, 0) / decimals_factor * price;
            
            positions.push(UserAssetPosition {
                asset: asset.token,
                symbol: asset.symbol.clone(),
                decimals: asset.decimals,
                a_token: asset.a_token,
                debt_token: asset.debt_token,
                collateral_balance,
                collateral_usd,
                debt_balance,
                debt_usd,
            });
        }
        
        Ok(positions)
    }
    
    /// Find the best collateral and debt assets for liquidation
    pub async fn find_liquidation_opportunity(
        &self,
        provider: &Provider<Http>,
        user: Address,
        chain: &str,
        prices: &HashMap<Address, f64>,
    ) -> anyhow::Result<Option<LiquidationOpportunity>> {
        let (total_collateral_usd, total_debt_usd, health_factor) = 
            self.get_user_data(provider, user).await?;
        
        if health_factor >= 1.0 || health_factor <= 0.0 {
            return Ok(None);
        }
        
        let positions = self.get_user_positions(provider, user, prices).await?;
        
        let best_collateral = positions
            .iter()
            .filter(|p| p.collateral_usd > 0.0)
            .max_by(|a, b| a.collateral_usd.partial_cmp(&b.collateral_usd).unwrap());
        
        let best_debt = positions
            .iter()
            .filter(|p| p.debt_usd > 0.0)
            .max_by(|a, b| a.debt_usd.partial_cmp(&b.debt_usd).unwrap());
        
        match (best_collateral, best_debt) {
            (Some(collateral), Some(debt)) => {
                let bonus = self.asset_configs
                    .get(&collateral.asset)
                    .map(|c| c.liquidation_bonus)
                    .unwrap_or(500);
                
                Ok(Some(LiquidationOpportunity {
                    user,
                    chain: chain.to_string(),
                    health_factor,
                    total_collateral_usd,
                    total_debt_usd,
                    best_collateral: collateral.clone(),
                    best_debt: debt.clone(),
                    liquidation_bonus: bonus,
                }))
            }
            _ => Ok(None),
        }
    }
}

/// Safely convert U256 to f64 with decimal scaling
fn u256_to_f64_safe(value: U256, decimals: u32) -> f64 {
    let max_safe = U256::from(1u128 << 100);
    
    if value >= max_safe {
        return 1_000_000.0;
    }
    
    if value > U256::from(u128::MAX) {
        let shifted = value >> 64;
        let shifted_low = shifted.low_u128();
        let divisor = 10u128.pow(decimals);
        let result = (shifted_low as f64) * (2.0_f64.powi(64)) / (divisor as f64);
        return result.min(1_000_000.0);
    }
    
    let low = value.low_u128();
    let divisor = 10u128.pow(decimals);
    low as f64 / divisor as f64
}
