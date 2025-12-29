//! Core types and data structures for the liquidator bot.

use ethers::types::{Address, U256};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// A position that may be liquidatable
#[derive(Debug, Clone)]
pub struct Position {
    pub user: Address,
    pub chain: String,
    pub protocol: Protocol,
    pub collateral_usd: f64,
    pub debt_usd: f64,
    pub health_factor: f64,
    pub liquidatable: bool,
}

impl Position {
    pub fn is_critical(&self) -> bool {
        !self.liquidatable && self.health_factor < 1.02 && self.health_factor > 0.0 && self.debt_usd > 1000.0
    }
}

/// Supported protocols
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Aave,
    Compound,
    Venus,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Aave => write!(f, "aave"),
            Protocol::Compound => write!(f, "compound"),
            Protocol::Venus => write!(f, "venus"),
        }
    }
}

/// Asset information
#[derive(Debug, Clone)]
pub struct Asset {
    pub symbol: String,
    pub token: Address,
    pub a_token: Address,
    pub debt_token: Address,
    pub decimals: u8,
}

/// Cached price data
#[derive(Debug, Clone)]
pub struct PriceData {
    pub price_usd: f64,
    pub timestamp: Instant,
}

impl PriceData {
    pub fn new(price_usd: f64) -> Self {
        Self {
            price_usd,
            timestamp: Instant::now(),
        }
    }
    
    pub fn is_stale(&self, max_age_ms: u64) -> bool {
        self.timestamp.elapsed().as_millis() > max_age_ms as u128
    }
}

/// Profit simulation result
#[derive(Debug, Clone)]
pub struct ProfitSimulation {
    pub profitable: bool,
    pub debt_to_repay: f64,
    pub collateral_received: f64,
    pub gross_profit: f64,
    pub flash_fee: f64,
    pub gas_cost_usd: f64,
    pub net_profit: f64,
}

impl ProfitSimulation {
    pub fn calculate(
        protocol: Protocol,
        debt_usd: f64,
        collateral_usd: f64,
        gas_cost_usd: f64,
        min_profit: f64,
    ) -> Self {
        let bonus = match protocol {
            Protocol::Aave => 0.05,      // 5%
            Protocol::Compound => 0.08,  // 8%
            Protocol::Venus => 0.10,     // 10%
        };
        
        let flash_loan_fee = 0.0009; // 0.09%
        let debt_to_repay = (debt_usd / 2.0).min(collateral_usd * 0.9);
        let collateral_received = debt_to_repay * (1.0 + bonus);
        let gross_profit = collateral_received - debt_to_repay;
        let flash_fee = debt_to_repay * flash_loan_fee;
        let net_profit = gross_profit - flash_fee - gas_cost_usd;
        
        Self {
            profitable: net_profit >= min_profit,
            debt_to_repay,
            collateral_received,
            gross_profit,
            flash_fee,
            gas_cost_usd,
            net_profit,
        }
    }
}

/// Stats tracking
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub events: u64,
    pub checks: u64,
    pub liquidations: u64,
    pub attempted: u64,
    pub failed: u64,
    pub skipped_unprofitable: u64,
    pub competitor_beats: u64,
    pub bad_debt: u64,
}

/// Circuit breaker state
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    pub consecutive_failures: u32,
    pub is_open: bool,
    pub open_until: Option<Instant>,
    pub threshold: u32,
    pub cooldown_ms: u64,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            is_open: false,
            open_until: None,
            threshold: 5,
            cooldown_ms: 300_000, // 5 minutes
        }
    }
}

impl CircuitBreaker {
    pub fn record_failure(&mut self) {
        self.consecutive_failures += 1;
        
        if self.consecutive_failures >= self.threshold && !self.is_open {
            self.is_open = true;
            self.open_until = Some(Instant::now() + std::time::Duration::from_millis(self.cooldown_ms));
            tracing::warn!("🔴 Circuit breaker OPEN - {} consecutive failures", self.consecutive_failures);
        }
    }
    
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        
        if self.is_open {
            if let Some(until) = self.open_until {
                if Instant::now() > until {
                    self.is_open = false;
                    self.open_until = None;
                    tracing::info!("🟢 Circuit breaker CLOSED");
                }
            }
        }
    }
    
    pub fn is_open(&self) -> bool {
        if !self.is_open {
            return false;
        }
        
        if let Some(until) = self.open_until {
            if Instant::now() > until {
                return false;
            }
        }
        
        true
    }
}

/// Execution lock with timeout
#[derive(Debug, Clone)]
pub struct ExecutionLock {
    pub timestamp: Instant,
    pub chain: String,
    pub protocol: Protocol,
}

impl ExecutionLock {
    pub fn new(chain: String, protocol: Protocol) -> Self {
        Self {
            timestamp: Instant::now(),
            chain,
            protocol,
        }
    }
    
    pub fn is_expired(&self, timeout_ms: u64) -> bool {
        self.timestamp.elapsed().as_millis() > timeout_ms as u128
    }
}
