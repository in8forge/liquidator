//! Position scanning and liquidation detection with full asset discovery.
//! Supports Aave V3, Compound V3, and Venus.
//! 
//! Features priority queue for crash days:
//! - Sort positions by profit potential (biggest first)
//! - Time-box scanning (stop after max time)
//! - Skip positions below profit threshold

use ethers::types::{Address, U256};
use std::sync::Arc;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use crate::chains::ChainManager;
use crate::protocols::aave::{AaveProtocol, LiquidationOpportunity};
use crate::protocols::compound::{CompoundProtocol, get_comet_addresses};
use crate::protocols::venus::VenusProtocol;
use crate::types::{Position, Protocol};
use crate::config::Config;
use crate::executor::Executor;
use crate::swap;

/// Maximum time to spend scanning per chain (seconds)
const MAX_SCAN_TIME_SECS: u64 = 10;

/// Maximum positions to process per scan cycle
const MAX_POSITIONS_PER_CYCLE: usize = 50;

/// Minimum debt to even consider (skip tiny positions)
const MIN_DEBT_THRESHOLD: f64 = 100.0;

/// Scanner handles position checking and liquidation execution
pub struct Scanner {
    pub chain_manager: Arc<ChainManager>,
    pub executor: Executor,
    pub min_profit_usd: f64,
    /// Cached Aave protocols per chain
    pub aave_protocols: tokio::sync::RwLock<HashMap<String, AaveProtocol>>,
    /// Cached Compound protocols per chain (multiple markets per chain)
    pub compound_protocols: tokio::sync::RwLock<HashMap<String, Vec<CompoundProtocol>>>,
    /// Cached Venus protocol (BNB only)
    pub venus_protocol: tokio::sync::RwLock<Option<VenusProtocol>>,
}

impl Scanner {
    pub fn new(chain_manager: Arc<ChainManager>, config: &Config) -> Self {
        Self {
            chain_manager,
            executor: Executor::new(
                config.dry_run,
                config.min_profit_usd,
                config.mev_threshold_usd,
            ),
            min_profit_usd: config.min_profit_usd,
            aave_protocols: tokio::sync::RwLock::new(HashMap::new()),
            compound_protocols: tokio::sync::RwLock::new(HashMap::new()),
            venus_protocol: tokio::sync::RwLock::new(None),
        }
    }
    
    /// Initialize Aave protocol for a chain
    pub async fn init_aave(&self, chain_name: &str) -> anyhow::Result<()> {
        let chain = match self.chain_manager.get_chain(chain_name) {
            Some(c) => c,
            None => return Ok(()),
        };
        
        let mut aave = AaveProtocol::new(
            &chain.config.pool_address,
            &chain.config.data_provider,
        )?;
        
        aave.discover_assets(&chain.provider()).await?;
        self.aave_protocols.write().await.insert(chain_name.to_string(), aave);
        
        Ok(())
    }
    
    /// Initialize Compound protocols for a chain
    pub async fn init_compound(&self, chain_name: &str) -> anyhow::Result<()> {
        let chain = match self.chain_manager.get_chain(chain_name) {
            Some(c) => c,
            None => return Ok(()),
        };
        
        let comet_addresses = get_comet_addresses(chain_name);
        if comet_addresses.is_empty() {
            return Ok(());
        }
        
        let mut protocols = Vec::new();
        
        for (market_name, base_token, comet_addr) in comet_addresses {
            let mut compound = CompoundProtocol::new(comet_addr, base_token);
            
            match compound.discover_assets(&chain.provider()).await {
                Ok(_) => {
                    info!("{}: Initialized Compound {} market", chain_name, market_name);
                    protocols.push(compound);
                }
                Err(e) => {
                    warn!("{}: Failed to init Compound {} - {}", chain_name, market_name, e);
                }
            }
        }
        
        if !protocols.is_empty() {
            self.compound_protocols.write().await.insert(chain_name.to_string(), protocols);
        }
        
        Ok(())
    }
    
    /// Get or create Aave protocol for a chain
    async fn get_aave_protocol(&self, chain_name: &str) -> Option<AaveProtocol> {
        {
            let protocols = self.aave_protocols.read().await;
            if let Some(aave) = protocols.get(chain_name) {
                return Some(aave.clone());
            }
        }
        
        if let Err(e) = self.init_aave(chain_name).await {
            error!("{}: Failed to initialize Aave - {}", chain_name, e);
            return None;
        }
        
        self.aave_protocols.read().await.get(chain_name).cloned()
    }
    
    /// Get Compound protocols for a chain
    async fn get_compound_protocols(&self, chain_name: &str) -> Vec<CompoundProtocol> {
        {
            let protocols = self.compound_protocols.read().await;
            if let Some(compounds) = protocols.get(chain_name) {
                return compounds.clone();
            }
        }
        
        if let Err(e) = self.init_compound(chain_name).await {
            debug!("{}: Failed to initialize Compound - {}", chain_name, e);
            return Vec::new();
        }
        
        self.compound_protocols.read().await
            .get(chain_name)
            .cloned()
            .unwrap_or_default()
    }
    
    /// Initialize Venus protocol (BNB only)
    pub async fn init_venus(&self) -> anyhow::Result<()> {
        let chain = match self.chain_manager.get_chain("bnb") {
            Some(c) => c,
            None => return Ok(()),
        };
        
        let mut venus = VenusProtocol::new();
        venus.discover_markets(&chain.provider()).await?;
        
        *self.venus_protocol.write().await = Some(venus);
        info!("bnb: Venus protocol initialized");
        
        Ok(())
    }
    
    /// Get Venus protocol
    async fn get_venus_protocol(&self) -> Option<VenusProtocol> {
        {
            let protocol = self.venus_protocol.read().await;
            if protocol.is_some() {
                return protocol.clone();
            }
        }
        
        if let Err(e) = self.init_venus().await {
            debug!("Failed to initialize Venus - {}", e);
            return None;
        }
        
        self.venus_protocol.read().await.clone()
    }
    
    /// Scan a chain for liquidatable positions (Aave + Compound + Venus)
    pub async fn scan_chain(&self, chain_name: &str, borrowers: &[Address]) -> Vec<Position> {
        let mut all_positions = Vec::new();
        
        // Scan Aave
        let aave_positions = self.scan_aave(chain_name, borrowers).await;
        all_positions.extend(aave_positions);
        
        // Scan Compound (uses same borrower list for now)
        let compound_positions = self.scan_compound(chain_name, borrowers).await;
        all_positions.extend(compound_positions);
        
        // Scan Venus (BNB only)
        if chain_name == "bnb" {
            let venus_positions = self.scan_venus(borrowers).await;
            all_positions.extend(venus_positions);
        }
        
        all_positions
    }
    
    /// Scan Aave positions
    async fn scan_aave(&self, chain_name: &str, borrowers: &[Address]) -> Vec<Position> {
        let chain = match self.chain_manager.get_chain(chain_name) {
            Some(c) => c,
            None => return Vec::new(),
        };
        
        if borrowers.is_empty() {
            return Vec::new();
        }
        
        let aave = match self.get_aave_protocol(chain_name).await {
            Some(a) => a,
            None => {
                match AaveProtocol::new(&chain.config.pool_address, &chain.config.data_provider) {
                    Ok(a) => a,
                    Err(e) => {
                        error!("{}: Failed to create Aave protocol - {}", chain_name, e);
                        return Vec::new();
                    }
                }
            }
        };
        
        let batch_size = 100;
        let mut all_positions = Vec::new();
        
        for batch in borrowers.chunks(batch_size) {
            match aave.batch_check_users(&chain.provider(), batch, chain_name).await {
                Ok(positions) => {
                    all_positions.extend(positions);
                }
                Err(e) => {
                    debug!("{}: Aave batch check failed - {}", chain_name, e);
                }
            }
        }
        
        all_positions
    }
    
    /// Scan Compound positions
    async fn scan_compound(&self, chain_name: &str, borrowers: &[Address]) -> Vec<Position> {
        let chain = match self.chain_manager.get_chain(chain_name) {
            Some(c) => c,
            None => return Vec::new(),
        };
        
        if borrowers.is_empty() {
            return Vec::new();
        }
        
        let compounds = self.get_compound_protocols(chain_name).await;
        if compounds.is_empty() {
            return Vec::new();
        }
        
        let mut all_positions = Vec::new();
        
        for compound in compounds {
            match compound.batch_check_users(&chain.provider(), borrowers, chain_name).await {
                Ok(positions) => {
                    all_positions.extend(positions);
                }
                Err(e) => {
                    debug!("{}: Compound {} check failed - {}", chain_name, compound.base_token_name, e);
                }
            }
        }
        
        all_positions
    }
    
    /// Scan Venus positions (BNB only)
    async fn scan_venus(&self, borrowers: &[Address]) -> Vec<Position> {
        let chain = match self.chain_manager.get_chain("bnb") {
            Some(c) => c,
            None => return Vec::new(),
        };
        
        if borrowers.is_empty() {
            return Vec::new();
        }
        
        let venus = match self.get_venus_protocol().await {
            Some(v) => v,
            None => return Vec::new(),
        };
        
        match venus.batch_check_users(&chain.provider(), borrowers).await {
            Ok(positions) => positions,
            Err(e) => {
                debug!("Venus check failed - {}", e);
                Vec::new()
            }
        }
    }
    
    /// Process detected positions
    /// Process positions with priority queue
    /// - Sort by estimated profit (debt * bonus - gas)
    /// - Process biggest opportunities first
    /// - Time-box to avoid missing opportunities
    pub async fn process_positions(&self, positions: Vec<Position>) {
        let start_time = Instant::now();
        let max_duration = Duration::from_secs(MAX_SCAN_TIME_SECS);
        
        // Filter and score positions
        let mut scored_positions: Vec<(f64, &Position)> = positions.iter()
            .filter(|p| p.liquidatable && p.debt_usd >= MIN_DEBT_THRESHOLD)
            .map(|p| {
                // Estimate profit score: debt * liquidation_bonus - estimated_gas
                // Higher score = higher priority
                let bonus = match p.protocol {
                    Protocol::Aave => 0.05,     // 5% typical Aave bonus
                    Protocol::Compound => 0.08, // 8% Compound discount
                    Protocol::Venus => 0.10,    // 10% Venus incentive
                };
                let estimated_gas = 5.0; // Rough $5 gas estimate
                let profit_score = (p.debt_usd * 0.5 * bonus) - estimated_gas; // 50% close factor
                (profit_score, p)
            })
            .filter(|(score, _)| *score > self.min_profit_usd)
            .collect();
        
        // Sort by profit score descending (highest first)
        scored_positions.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        
        let total_liquidatable = scored_positions.len();
        
        if total_liquidatable == 0 {
            // Still log critical positions for monitoring
            let critical: Vec<&Position> = positions.iter()
                .filter(|p| p.is_critical())
                .collect();
            
            for pos in critical {
                debug!(
                    "🚨 CRITICAL: {} {} {:?} | ${:.0} | HF: {:.4}",
                    pos.chain, pos.protocol, pos.user, pos.debt_usd, pos.health_factor
                );
            }
            return;
        }
        
        info!(
            "🔥 {} LIQUIDATABLE (sorted by profit, processing top {})",
            total_liquidatable,
            MAX_POSITIONS_PER_CYCLE.min(total_liquidatable)
        );
        
        // Log top opportunities
        for (i, (score, pos)) in scored_positions.iter().take(5).enumerate() {
            info!(
                "  #{}: {} {} ${:.0} debt | ~${:.2} profit | HF {:.4}",
                i + 1, pos.chain, pos.protocol, pos.debt_usd, score, pos.health_factor
            );
        }
        
        let mut processed = 0;
        let mut successful = 0;
        
        for (score, pos) in scored_positions.into_iter().take(MAX_POSITIONS_PER_CYCLE) {
            // Time-box check
            if start_time.elapsed() > max_duration {
                warn!(
                    "⏱️ Time limit reached after {} positions ({} successful)",
                    processed, successful
                );
                break;
            }
            
            // Circuit breaker check
            if self.chain_manager.is_circuit_open() {
                info!("⏸️ Circuit breaker open, stopping");
                break;
            }
            
            debug!(
                "Processing #{}: {:?} | ${:.0} | ~${:.2} profit",
                processed + 1, pos.user, pos.debt_usd, score
            );
            
            let result = self.process_liquidatable_prioritized(pos).await;
            processed += 1;
            
            if result {
                successful += 1;
            }
        }
        
        if processed > 0 {
            info!(
                "📊 Processed {}/{} positions in {:?} ({} successful)",
                processed, total_liquidatable, start_time.elapsed(), successful
            );
        }
    }
    
    /// Process a single liquidatable position (returns true if successful)
    async fn process_liquidatable_prioritized(&self, pos: &Position) -> bool {
        let lock_key = format!("{}-{}-{:?}", pos.protocol, pos.chain, pos.user);
        
        if !self.chain_manager.acquire_lock(&lock_key, &pos.chain, pos.protocol) {
            debug!("Lock already held for {}", lock_key);
            return false;
        }
        
        let result = match pos.protocol {
            Protocol::Aave => self.process_aave_liquidation(pos).await,
            Protocol::Compound => {
                self.process_compound_liquidation(pos).await;
                false // Compound doesn't return success yet
            }
            Protocol::Venus => {
                self.process_venus_liquidation(pos).await;
                false // Venus doesn't return success yet
            }
        };
        
        self.chain_manager.release_lock(&lock_key);
        result
    }
    
    /// Process Aave liquidation with full asset discovery (returns true if executed)
    async fn process_aave_liquidation(&self, pos: &Position) -> bool {
        let chain = match self.chain_manager.get_chain(&pos.chain) {
            Some(c) => c,
            None => return false,
        };
        
        let aave = match self.get_aave_protocol(&pos.chain).await {
            Some(a) => a,
            None => {
                warn!("Could not get Aave protocol for {}", pos.chain);
                return false;
            }
        };
        
        let prices: HashMap<Address, f64> = chain.prices
            .iter()
            .map(|entry| (*entry.key(), entry.value().price_usd))
            .collect();
        
        let opportunity = match aave.find_liquidation_opportunity(
            &chain.provider(),
            pos.user,
            &pos.chain,
            &prices,
        ).await {
            Ok(Some(opp)) => opp,
            Ok(None) => {
                debug!("  ⚠️ No liquidation opportunity found");
                return false;
            }
            Err(e) => {
                warn!("  ❌ Failed to analyze position: {}", e);
                return false;
            }
        };
        
        info!(
            "💰 {} {:?} | {} ${:.0} -> {} ${:.0} | HF {:.4}",
            pos.chain, pos.user,
            opportunity.best_collateral.symbol,
            opportunity.best_collateral.collateral_usd,
            opportunity.best_debt.symbol,
            opportunity.best_debt.debt_usd,
            pos.health_factor
        );
        
        let gas_cost = self.executor
            .estimate_gas_cost_usd(&chain, 800_000)
            .await
            .unwrap_or(5.0);
        
        let bonus_pct = opportunity.liquidation_bonus as f64 / 10000.0;
        let debt_to_cover = opportunity.best_debt.debt_usd * 0.5;
        let collateral_received = debt_to_cover * (1.0 + bonus_pct);
        let gross_profit = collateral_received - debt_to_cover;
        let flash_fee = debt_to_cover * 0.0009;
        let net_profit = gross_profit - flash_fee - gas_cost;
        
        if net_profit < self.min_profit_usd {
            debug!(
                "   ⏭️ Skipping unprofitable (net: ${:.2}, gas: ${:.2})",
                net_profit, gas_cost
            );
            self.chain_manager.stats.write().skipped_unprofitable += 1;
            return false;
        }
        
        info!(
            "   ✅ Profitable! ~${:.2} (bonus: {:.1}%, gas: ${:.2})",
            net_profit, bonus_pct * 100.0, gas_cost
        );
        
        // Validate swap path
        let debt_to_cover_wei = U256::from((debt_to_cover * 10_f64.powi(opportunity.best_debt.decimals as i32)) as u128);
        
        let swap_quote = swap::validate_liquidation_swap(
            &chain.provider(),
            &pos.chain,
            opportunity.best_collateral.asset,
            opportunity.best_debt.asset,
            opportunity.best_collateral.collateral_balance,
            debt_to_cover_wei,
            opportunity.liquidation_bonus,
        ).await;
        
        match swap_quote {
            Ok(Some(quote)) => {
                debug!(
                    "   🔄 Swap path: {} hop(s)",
                    quote.path.len() - 1
                );
            }
            Ok(None) => {
                warn!("   ❌ No swap path - skipping");
                self.chain_manager.stats.write().skipped_unprofitable += 1;
                return false;
            }
            Err(e) => {
                debug!("   ⚠️ Swap check failed: {} - attempting anyway", e);
            }
        }
        
        self.chain_manager.stats.write().attempted += 1;
        self.execute_aave_liquidation(&opportunity, debt_to_cover_wei).await
    }
    
    /// Process Compound liquidation
    async fn process_compound_liquidation(&self, pos: &Position) {
        let chain = match self.chain_manager.get_chain(&pos.chain) {
            Some(c) => c,
            None => return,
        };
        
        let compounds = self.get_compound_protocols(&pos.chain).await;
        
        // Find which Comet market this user is in
        for compound in compounds {
            if let Ok(Some(details)) = compound.get_liquidation_details(&chain.provider(), pos.user).await {
                info!(
                    "   📊 Compound {} market: ${:.0} borrow, {} collateral assets",
                    details.base_token,
                    details.borrow_usd,
                    details.collaterals.len()
                );
                
                // Estimate profit (Compound uses absorb mechanism - liquidator gets collateral discount)
                let gas_cost = self.executor
                    .estimate_gas_cost_usd(&chain, 500_000)
                    .await
                    .unwrap_or(3.0);
                
                // Compound typically offers ~5-8% discount on collateral
                let discount = 0.05;
                let gross_profit = pos.collateral_usd * discount;
                let net_profit = gross_profit - gas_cost;
                
                if net_profit < self.min_profit_usd {
                    info!("   ⏭️ Skipping unprofitable Compound (net: ${:.2})", net_profit);
                    self.chain_manager.stats.write().skipped_unprofitable += 1;
                    return;
                }
                
                info!("   ✅ Compound profitable! Expected: ${:.2}", net_profit);
                
                self.chain_manager.stats.write().attempted += 1;
                
                if self.executor.dry_run {
                    info!("   🧪 DRY RUN: Would absorb Compound position");
                    info!("      Comet: {:?}", compound.comet_address);
                    info!("      User: {:?}", pos.user);
                    self.chain_manager.record_success();
                } else {
                    // TODO: Implement actual Compound absorb call
                    warn!("   ⚠️ Compound execution not yet implemented");
                    self.chain_manager.record_failure();
                }
                
                return;
            }
        }
        
        warn!("   Could not find Compound market for user");
    }
    
    /// Process Venus liquidation
    async fn process_venus_liquidation(&self, pos: &Position) {
        let chain = match self.chain_manager.get_chain("bnb") {
            Some(c) => c,
            None => return,
        };
        
        let venus = match self.get_venus_protocol().await {
            Some(v) => v,
            None => {
                warn!("Could not get Venus protocol");
                return;
            }
        };
        
        // Get detailed position info
        if let Ok(Some(details)) = venus.get_liquidation_details(&chain.provider(), pos.user).await {
            info!(
                "   📊 Venus: ${:.0} collateral, ${:.0} borrow, ${:.0} shortfall",
                details.total_collateral_usd,
                details.total_borrow_usd,
                details.shortfall
            );
            
            // Find best market to liquidate (highest borrow)
            let best_borrow_market = details.markets.iter()
                .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap());
            
            let best_collateral_market = details.markets.iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            
            if let (Some(borrow), Some(collateral)) = (best_borrow_market, best_collateral_market) {
                info!(
                    "   Best borrow: {:?} ${:.0} | Best collateral: {:?} ${:.0}",
                    borrow.0, borrow.2, collateral.0, collateral.1
                );
            }
            
            // Estimate profit
            let gas_cost = self.executor
                .estimate_gas_cost_usd(&chain, 600_000)
                .await
                .unwrap_or(2.0);
            
            // Venus liquidation incentive (typically 10%)
            let incentive = venus.liquidation_incentive - 1.0;
            let repay_amount = pos.debt_usd * venus.close_factor;
            let gross_profit = repay_amount * incentive;
            let net_profit = gross_profit - gas_cost;
            
            if net_profit < self.min_profit_usd {
                info!("   ⏭️ Skipping unprofitable Venus (net: ${:.2})", net_profit);
                self.chain_manager.stats.write().skipped_unprofitable += 1;
                return;
            }
            
            info!(
                "   ✅ Venus profitable! Expected: ${:.2} (incentive: {:.0}%)",
                net_profit, incentive * 100.0
            );
            
            self.chain_manager.stats.write().attempted += 1;
            
            if self.executor.dry_run {
                info!("   🧪 DRY RUN: Would execute Venus liquidation");
                info!("      User: {:?}", pos.user);
                info!("      Repay: ${:.0}", repay_amount);
                self.chain_manager.record_success();
            } else {
                // TODO: Implement actual Venus liquidateBorrow call
                warn!("   ⚠️ Venus execution not yet implemented");
                self.chain_manager.record_failure();
            }
        } else {
            warn!("   Could not get Venus liquidation details");
        }
    }
    
    /// Execute Aave liquidation (returns true if successful)
    async fn execute_aave_liquidation(&self, opportunity: &LiquidationOpportunity, debt_to_cover: U256) -> bool {
        let chain = match self.chain_manager.get_chain(&opportunity.chain) {
            Some(c) => c,
            None => return false,
        };
        
        let aave = match self.get_aave_protocol(&opportunity.chain).await {
            Some(a) => a,
            None => return false,
        };
        
        match aave.get_user_data(&chain.provider(), opportunity.user).await {
            Ok((_, _, hf)) => {
                if hf >= 1.0 {
                    info!("   👀 Competitor beat us (HF now: {:.4})", hf);
                    self.chain_manager.stats.write().competitor_beats += 1;
                    return false;
                }
            }
            Err(e) => {
                warn!("   Failed to verify position: {}", e);
            }
        }
        
        if self.executor.dry_run {
            info!("   🧪 DRY RUN: Would liquidate {:?}", opportunity.user);
            self.chain_manager.record_success();
            return true;
        }
        
        match self.executor.execute_aave_liquidation(
            &chain,
            &Position {
                user: opportunity.user,
                chain: opportunity.chain.clone(),
                protocol: Protocol::Aave,
                collateral_usd: opportunity.total_collateral_usd,
                debt_usd: opportunity.total_debt_usd,
                health_factor: opportunity.health_factor,
                liquidatable: true,
            },
            opportunity.best_collateral.asset,
            opportunity.best_debt.asset,
            debt_to_cover,
        ).await {
            Ok(Some(tx_hash)) => {
                info!("   ✅ TX: {:?}", tx_hash);
                self.chain_manager.record_success();
                true
            }
            Ok(None) => {
                warn!("   ⚠️ Not executed");
                self.chain_manager.record_failure();
                false
            }
            Err(e) => {
                error!("   ❌ Failed: {}", e);
                self.chain_manager.record_failure();
                false
            }
        }
    }
}
