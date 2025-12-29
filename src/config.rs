//! Configuration management for the liquidator bot.

use serde::Deserialize;
use std::env;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Missing required environment variable: {0}")]
    MissingEnvVar(String),
    
    #[error("Invalid private key format")]
    InvalidPrivateKey,
    
    #[error("Failed to parse config: {0}")]
    ParseError(String),
}

#[derive(Clone, Debug)]
pub struct ChainConfig {
    pub name: String,
    pub rpc_url: String,
    pub ws_url: Option<String>,
    pub pool_address: String,
    pub data_provider: String,
    pub liquidator_address: Option<String>,
    pub chain_id: u64,
    pub gas_limit: u64,
    pub native_price_fallback: f64,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub private_key: String,
    pub discord_webhook: Option<String>,
    pub dry_run: bool,
    pub health_port: u16,
    pub min_profit_usd: f64,
    pub mev_threshold_usd: f64,
    pub price_cache_ms: u64,
    pub owner_wallet: String,
    pub chains: Vec<ChainConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        dotenv::dotenv().ok();
        
        // Required
        let private_key = env::var("PRIVATE_KEY")
            .map_err(|_| ConfigError::MissingEnvVar("PRIVATE_KEY".to_string()))?;
        
        // Validate private key format
        let pk_clean = private_key.strip_prefix("0x").unwrap_or(&private_key);
        if pk_clean.len() != 64 || !pk_clean.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ConfigError::InvalidPrivateKey);
        }
        
        // Optional with defaults
        let discord_webhook = env::var("DISCORD_WEBHOOK").ok();
        let dry_run = env::var("DRY_RUN").map(|v| v == "true").unwrap_or(false);
        let health_port = env::var("HEALTH_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3847);
        let min_profit_usd = env::var("MIN_PROFIT_USD")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5.0);
        let mev_threshold_usd = env::var("MEV_THRESHOLD_USD")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(500.0);
        let price_cache_ms = env::var("PRICE_CACHE_MS")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(10000);
        let owner_wallet = env::var("OWNER_WALLET")
            .unwrap_or_else(|_| "0x55F5F2186f907057EB40a9EFEa99A0A41BcbB885".to_string());
        
        // Build chain configs
        let mut chains = Vec::new();
        
        // Base
        if let Ok(rpc) = env::var("BASE_RPC_URL") {
            chains.push(ChainConfig {
                name: "base".to_string(),
                rpc_url: rpc,
                ws_url: env::var("BASE_WS_URL").ok(),
                pool_address: "0xA238Dd80C259a72e81d7e4664a9801593F98d1c5".to_string(),
                data_provider: "0x2d8A3C5677189723C4cB8873CfC9C8976FDF38Ac".to_string(),
                liquidator_address: env::var("BASE_LIQUIDATOR").ok(),
                chain_id: 8453,
                gas_limit: 800_000,
                native_price_fallback: 3000.0,
            });
        }
        
        // Polygon
        if let Ok(rpc) = env::var("POLYGON_RPC_URL") {
            chains.push(ChainConfig {
                name: "polygon".to_string(),
                rpc_url: rpc,
                ws_url: env::var("POLYGON_WS_URL").ok(),
                pool_address: "0x794a61358D6845594F94dc1DB02A252b5b4814aD".to_string(),
                data_provider: "0x69FA688f1Dc47d4B5d8029D5a35FB7a548310654".to_string(),
                liquidator_address: env::var("POLYGON_LIQUIDATOR").ok(),
                chain_id: 137,
                gas_limit: 800_000,
                native_price_fallback: 0.5,
            });
        }
        
        // Arbitrum
        if let Ok(rpc) = env::var("ARBITRUM_RPC_URL") {
            chains.push(ChainConfig {
                name: "arbitrum".to_string(),
                rpc_url: rpc,
                ws_url: env::var("ARBITRUM_WS_URL").ok(),
                pool_address: "0x794a61358D6845594F94dc1DB02A252b5b4814aD".to_string(),
                data_provider: "0x69FA688f1Dc47d4B5d8029D5a35FB7a548310654".to_string(),
                liquidator_address: env::var("ARBITRUM_LIQUIDATOR").ok(),
                chain_id: 42161,
                gas_limit: 1_500_000,
                native_price_fallback: 3000.0,
            });
        }
        
        // Avalanche
        if let Ok(rpc) = env::var("AVALANCHE_RPC_URL") {
            chains.push(ChainConfig {
                name: "avalanche".to_string(),
                rpc_url: rpc,
                ws_url: env::var("AVALANCHE_WS_URL").ok(),
                pool_address: "0x794a61358D6845594F94dc1DB02A252b5b4814aD".to_string(),
                data_provider: "0x69FA688f1Dc47d4B5d8029D5a35FB7a548310654".to_string(),
                liquidator_address: env::var("AVALANCHE_LIQUIDATOR").ok(),
                chain_id: 43114,
                gas_limit: 800_000,
                native_price_fallback: 35.0,
            });
        }
        
        // BNB Chain
        if let Ok(rpc) = env::var("BNB_RPC_URL") {
            chains.push(ChainConfig {
                name: "bnb".to_string(),
                rpc_url: rpc,
                ws_url: env::var("BNB_WS_URL").ok(),
                pool_address: "0xfD36E2c2a6789Db23113685031d7F16329158384".to_string(), // Venus Comptroller
                data_provider: "".to_string(),
                liquidator_address: env::var("BNB_LIQUIDATOR").ok(),
                chain_id: 56,
                gas_limit: 1_500_000,
                native_price_fallback: 600.0,
            });
        }
        
        Ok(Config {
            private_key,
            discord_webhook,
            dry_run,
            health_port,
            min_profit_usd,
            mev_threshold_usd,
            price_cache_ms,
            owner_wallet,
            chains,
        })
    }
}
