//! Venus protocol implementation for BNB Chain.
//!
//! Venus uses a Comptroller + vToken model similar to Compound V2.
//! Users supply assets to get vTokens and can borrow against them.

use ethers::prelude::*;
use ethers::providers::{Provider, Http};
use ethers::types::Address;
use std::sync::Arc;
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::types::{Position, Protocol};

// Venus Comptroller ABI
abigen!(
    IVenusComptroller,
    r#"[
        function getAccountLiquidity(address account) external view returns (uint256 error, uint256 liquidity, uint256 shortfall)
        function getAllMarkets() external view returns (address[] memory)
        function markets(address vToken) external view returns (bool isListed, uint256 collateralFactorMantissa, bool isVenus)
        function closeFactorMantissa() external view returns (uint256)
        function liquidationIncentiveMantissa() external view returns (uint256)
    ]"#
);

// Venus vToken ABI
abigen!(
    IVToken,
    r#"[
        function balanceOf(address owner) external view returns (uint256)
        function borrowBalanceStored(address account) external view returns (uint256)
        function borrowBalanceCurrent(address account) external returns (uint256)
        function underlying() external view returns (address)
        function symbol() external view returns (string memory)
        function decimals() external view returns (uint8)
        function exchangeRateStored() external view returns (uint256)
    ]"#
);

/// Venus Comptroller address on BNB Chain
pub const VENUS_COMPTROLLER: &str = "0xfD36E2c2a6789Db23113685031d7F16329158384";

/// Venus market info
#[derive(Debug, Clone)]
pub struct VenusMarket {
    pub v_token: Address,
    pub underlying: Address,
    pub symbol: String,
    pub decimals: u8,
    pub collateral_factor: f64,
}

/// User's Venus position
#[derive(Debug, Clone)]
pub struct VenusPosition {
    pub user: Address,
    pub total_collateral_usd: f64,
    pub total_borrow_usd: f64,
    pub shortfall: f64,
    pub markets: Vec<(Address, f64, f64)>, // (vToken, supply_usd, borrow_usd)
}

#[derive(Clone)]
pub struct VenusProtocol {
    pub comptroller: Address,
    pub markets: Vec<VenusMarket>,
    pub liquidation_incentive: f64,
    pub close_factor: f64,
}

impl VenusProtocol {
    pub fn new() -> Self {
        Self {
            comptroller: VENUS_COMPTROLLER.parse().unwrap(),
            markets: Vec::new(),
            liquidation_incentive: 1.1, // 10% bonus default
            close_factor: 0.5, // 50% default
        }
    }
    
    /// Discover all Venus markets
    pub async fn discover_markets(&mut self, provider: &Provider<Http>) -> anyhow::Result<()> {
        let comptroller = IVenusComptroller::new(self.comptroller, Arc::new(provider.clone()));
        
        // Get liquidation parameters
        if let Ok(incentive) = comptroller.liquidation_incentive_mantissa().call().await {
            self.liquidation_incentive = incentive.as_u128() as f64 / 1e18;
        }
        
        if let Ok(close_factor) = comptroller.close_factor_mantissa().call().await {
            self.close_factor = close_factor.as_u128() as f64 / 1e18;
        }
        
        // Get all markets
        let market_addresses = comptroller.get_all_markets().call().await?;
        info!("Venus has {} markets", market_addresses.len());
        
        for v_token_addr in market_addresses {
            let v_token = IVToken::new(v_token_addr, Arc::new(provider.clone()));
            
            // Get market info from comptroller
            let market_info = match comptroller.markets(v_token_addr).call().await {
                Ok(info) => info,
                Err(_) => continue,
            };
            
            if !market_info.0 { // isListed
                continue;
            }
            
            let collateral_factor = market_info.1.as_u128() as f64 / 1e18;
            
            // Get token details
            let symbol = v_token.symbol().call().await.unwrap_or_else(|_| "???".to_string());
            let decimals = v_token.decimals().call().await.unwrap_or(18);
            
            // Get underlying (vBNB doesn't have underlying)
            let underlying = v_token.underlying().call().await.unwrap_or(Address::zero());
            
            self.markets.push(VenusMarket {
                v_token: v_token_addr,
                underlying,
                symbol: symbol.clone(),
                decimals,
                collateral_factor,
            });
            
            debug!("  {} (CF: {:.0}%)", symbol, collateral_factor * 100.0);
        }
        
        info!("Discovered {} Venus markets", self.markets.len());
        Ok(())
    }
    
    /// Check if a user has shortfall (liquidatable)
    pub async fn check_user(
        &self,
        provider: &Provider<Http>,
        user: Address,
    ) -> anyhow::Result<Option<Position>> {
        let comptroller = IVenusComptroller::new(self.comptroller, Arc::new(provider.clone()));
        
        let (error, liquidity, shortfall) = comptroller.get_account_liquidity(user).call().await?;
        
        // Error check
        if !error.is_zero() {
            return Ok(None);
        }
        
        // No shortfall = not liquidatable
        if shortfall.is_zero() {
            return Ok(None);
        }
        
        let shortfall_usd = shortfall.as_u128() as f64 / 1e18;
        
        // Skip small positions
        if shortfall_usd < 10.0 {
            return Ok(None);
        }
        
        // Calculate approximate totals
        let (total_collateral, total_borrow) = self.get_user_totals(provider, user).await?;
        
        // Health factor approximation
        let health_factor = if total_borrow > 0.0 {
            total_collateral / total_borrow
        } else {
            999.0
        };
        
        Ok(Some(Position {
            user,
            chain: "bnb".to_string(),
            protocol: Protocol::Venus,
            collateral_usd: total_collateral,
            debt_usd: total_borrow,
            health_factor,
            liquidatable: true, // shortfall > 0 means liquidatable
        }))
    }
    
    /// Get user's total collateral and borrow values
    async fn get_user_totals(
        &self,
        provider: &Provider<Http>,
        user: Address,
    ) -> anyhow::Result<(f64, f64)> {
        let mut total_collateral = 0.0;
        let mut total_borrow = 0.0;
        
        for market in &self.markets {
            let v_token = IVToken::new(market.v_token, Arc::new(provider.clone()));
            
            // Get supply balance
            let v_token_balance = v_token.balance_of(user).call().await.unwrap_or(U256::zero());
            if !v_token_balance.is_zero() {
                // Convert vToken balance to underlying using exchange rate
                let exchange_rate = v_token.exchange_rate_stored().call().await.unwrap_or(U256::from(1e18 as u64));
                let underlying_balance = v_token_balance * exchange_rate / U256::from(1e18 as u64);
                
                // Assume $1 per token for now (would need oracle for accurate pricing)
                let supply_usd = underlying_balance.as_u128() as f64 / 10_f64.powi(market.decimals as i32);
                total_collateral += supply_usd * market.collateral_factor;
            }
            
            // Get borrow balance
            let borrow_balance = v_token.borrow_balance_stored(user).call().await.unwrap_or(U256::zero());
            if !borrow_balance.is_zero() {
                let borrow_usd = borrow_balance.as_u128() as f64 / 10_f64.powi(market.decimals as i32);
                total_borrow += borrow_usd;
            }
        }
        
        Ok((total_collateral, total_borrow))
    }
    
    /// Batch check multiple users using Multicall3
    pub async fn batch_check_users(
        &self,
        provider: &Provider<Http>,
        users: &[Address],
    ) -> anyhow::Result<Vec<Position>> {
        if users.is_empty() {
            return Ok(Vec::new());
        }
        
        let multicall_addr: Address = "0xcA11bde05977b3631167028862bE2a173976CA11".parse()?;
        let mut all_positions = Vec::new();
        
        // Process in batches of 100
        for batch in users.chunks(100) {
            match self.multicall_check_users(provider, multicall_addr, batch).await {
                Ok(positions) => {
                    all_positions.extend(positions);
                }
                Err(e) => {
                    // Fallback to sequential
                    debug!("Venus multicall failed, falling back: {}", e);
                    for user in batch {
                        if let Ok(Some(pos)) = self.check_user(provider, *user).await {
                            all_positions.push(pos);
                        }
                    }
                }
            }
        }
        
        debug!("Venus: Checked {} users, found {} liquidatable", users.len(), all_positions.len());
        Ok(all_positions)
    }
    
    /// Use Multicall3 to batch check getAccountLiquidity
    async fn multicall_check_users(
        &self,
        provider: &Provider<Http>,
        multicall_addr: Address,
        users: &[Address],
    ) -> anyhow::Result<Vec<Position>> {
        use ethers::abi::{Function, Param, ParamType, Token, StateMutability};
        
        // getAccountLiquidity function
        let get_liq_fn = Function {
            name: "getAccountLiquidity".to_string(),
            inputs: vec![Param {
                name: "account".to_string(),
                kind: ParamType::Address,
                internal_type: None,
            }],
            outputs: vec![
                Param { name: "error".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "liquidity".to_string(), kind: ParamType::Uint(256), internal_type: None },
                Param { name: "shortfall".to_string(), kind: ParamType::Uint(256), internal_type: None },
            ],
            constant: None,
            state_mutability: StateMutability::View,
        };
        
        // Build calls
        let mut targets = Vec::with_capacity(users.len());
        let mut call_data = Vec::with_capacity(users.len());
        
        for user in users {
            let encoded = get_liq_fn.encode_input(&[Token::Address(*user)])?;
            targets.push(self.comptroller);
            call_data.push(ethers::types::Bytes::from(encoded));
        }
        
        // abigen for Multicall3
        abigen!(
            IMulticall3Local,
            r#"[
                function aggregate(address[] calldata targets, bytes[] calldata data) external payable returns (uint256 blockNumber, bytes[] memory returnData)
            ]"#
        );
        
        let multicall = IMulticall3Local::new(multicall_addr, Arc::new(provider.clone()));
        let (_, results) = multicall.aggregate(targets, call_data).call().await?;
        
        let mut positions = Vec::new();
        
        for (i, result_bytes) in results.iter().enumerate() {
            if result_bytes.len() < 96 {
                continue;
            }
            
            let decoded = get_liq_fn.decode_output(result_bytes)?;
            
            let error = match decoded.get(0) {
                Some(Token::Uint(v)) => *v,
                _ => continue,
            };
            
            let shortfall = match decoded.get(2) {
                Some(Token::Uint(v)) => *v,
                _ => continue,
            };
            
            // Skip if error or no shortfall
            if !error.is_zero() || shortfall.is_zero() {
                continue;
            }
            
            // Has shortfall - fetch full details
            if let Ok(Some(pos)) = self.check_user(provider, users[i]).await {
                positions.push(pos);
            }
        }
        
        Ok(positions)
    }
    
    /// Get detailed liquidation info for a user
    pub async fn get_liquidation_details(
        &self,
        provider: &Provider<Http>,
        user: Address,
    ) -> anyhow::Result<Option<VenusPosition>> {
        let comptroller = IVenusComptroller::new(self.comptroller, Arc::new(provider.clone()));
        
        let (error, _liquidity, shortfall) = comptroller.get_account_liquidity(user).call().await?;
        
        if !error.is_zero() || shortfall.is_zero() {
            return Ok(None);
        }
        
        let shortfall_usd = shortfall.as_u128() as f64 / 1e18;
        
        let mut markets_data = Vec::new();
        let mut total_collateral = 0.0;
        let mut total_borrow = 0.0;
        
        for market in &self.markets {
            let v_token = IVToken::new(market.v_token, Arc::new(provider.clone()));
            
            let v_token_balance = v_token.balance_of(user).call().await.unwrap_or(U256::zero());
            let borrow_balance = v_token.borrow_balance_stored(user).call().await.unwrap_or(U256::zero());
            
            if v_token_balance.is_zero() && borrow_balance.is_zero() {
                continue;
            }
            
            let exchange_rate = v_token.exchange_rate_stored().call().await.unwrap_or(U256::from(1e18 as u64));
            let underlying_balance = v_token_balance * exchange_rate / U256::from(1e18 as u64);
            
            let supply_usd = underlying_balance.as_u128() as f64 / 10_f64.powi(market.decimals as i32);
            let borrow_usd = borrow_balance.as_u128() as f64 / 10_f64.powi(market.decimals as i32);
            
            total_collateral += supply_usd;
            total_borrow += borrow_usd;
            
            if supply_usd > 0.0 || borrow_usd > 0.0 {
                markets_data.push((market.v_token, supply_usd, borrow_usd));
            }
        }
        
        Ok(Some(VenusPosition {
            user,
            total_collateral_usd: total_collateral,
            total_borrow_usd: total_borrow,
            shortfall: shortfall_usd,
            markets: markets_data,
        }))
    }
}

/// Discover Venus borrowers from Borrow events
pub async fn discover_venus_borrowers(
    provider: &Provider<Http>,
    from_block: u64,
    to_block: Option<u64>,
) -> anyhow::Result<Vec<Address>> {
    use std::collections::HashSet;
    
    let current_block = provider.get_block_number().await?.as_u64();
    let to_block = to_block.unwrap_or(current_block);
    
    // Borrow event topic (Compound V2 style)
    // Borrow(address borrower, uint256 borrowAmount, uint256 accountBorrows, uint256 totalBorrows)
    let borrow_topic: H256 = "0x13ed6866d4e1ee6da46f845c46d7e54120883d75c5ea9a2dacc1c4ca8984ab80".parse()?;
    
    let comptroller: Address = VENUS_COMPTROLLER.parse()?;
    let comptroller_contract = IVenusComptroller::new(comptroller, Arc::new(provider.clone()));
    
    // Get all markets to scan
    let markets = comptroller_contract.get_all_markets().call().await?;
    
    let mut all_borrowers: HashSet<Address> = HashSet::new();
    
    let chunk_size = 10_000u64;
    
    for market in markets {
        let mut start = from_block;
        
        while start < to_block {
            let end = (start + chunk_size).min(to_block);
            
            let filter = Filter::new()
                .address(market)
                .topic0(borrow_topic)
                .from_block(start)
                .to_block(end);
            
            match provider.get_logs(&filter).await {
                Ok(logs) => {
                    for log in logs {
                        // borrower is topic[1]
                        if log.topics.len() > 1 {
                            let borrower = Address::from_slice(&log.topics[1].as_bytes()[12..]);
                            all_borrowers.insert(borrower);
                        }
                    }
                }
                Err(e) => {
                    debug!("Venus: Error fetching logs for {:?}: {}", market, e);
                }
            }
            
            start = end + 1;
        }
    }
    
    let borrowers: Vec<Address> = all_borrowers.into_iter().collect();
    info!("bnb: Discovered {} Venus borrowers", borrowers.len());
    
    Ok(borrowers)
}
