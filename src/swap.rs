//! Multi-DEX swap routing for maximum liquidity during crashes.
//!
//! Priority order:
//! 1. 1inch Fusion API (best aggregation, MEV protection)
//! 2. Paraswap API (good liquidity, fast)
//! 3. Uniswap V3 on-chain quote (fallback)
//!
//! During Oct 10-style events, Uniswap liquidity dries up.
//! Aggregators route through multiple DEXs for better execution.

use ethers::prelude::*;
use ethers::providers::{Provider, Http};
use ethers::types::{Address, U256};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

// Uniswap V3 Quoter ABI
abigen!(
    IQuoterV2,
    r#"[
        function quoteExactInputSingle(address tokenIn, address tokenOut, uint24 fee, uint256 amountIn, uint160 sqrtPriceLimitX96) external returns (uint256 amountOut)
    ]"#
);

/// Chain IDs for aggregator APIs
fn get_chain_id(chain: &str) -> Option<u64> {
    match chain {
        "ethereum" => Some(1),
        "base" => Some(8453),
        "polygon" => Some(137),
        "arbitrum" => Some(42161),
        "avalanche" => Some(43114),
        "bnb" => Some(56),
        _ => None,
    }
}

/// Uniswap V3 Quoter addresses by chain
fn get_quoter_address(chain: &str) -> Option<Address> {
    let addr = match chain {
        "base" => "0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a",
        "polygon" => "0x61fFE014bA17989E743c5F6cB21bF9697530B21e",
        "arbitrum" => "0x61fFE014bA17989E743c5F6cB21bF9697530B21e",
        "avalanche" => "0xbe0F5544EC67e9B3b2D979aaA43f18Fd87E6257F",
        "bnb" => "0x78D78E420Da98ad378D7799bE8f4AF69033EB077",
        _ => return None,
    };
    addr.parse().ok()
}

/// Common pool fees for Uniswap V3
pub const FEE_LOWEST: u32 = 100;
pub const FEE_LOW: u32 = 500;
pub const FEE_MEDIUM: u32 = 3000;
pub const FEE_HIGH: u32 = 10000;

/// DEX source for the quote
#[derive(Debug, Clone, PartialEq)]
pub enum DexSource {
    OneInch,
    Paraswap,
    UniswapV3,
}

impl std::fmt::Display for DexSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DexSource::OneInch => write!(f, "1inch"),
            DexSource::Paraswap => write!(f, "Paraswap"),
            DexSource::UniswapV3 => write!(f, "Uniswap V3"),
        }
    }
}

/// Swap quote with source information
#[derive(Debug, Clone)]
pub struct SwapQuote {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee: u32,
    pub price_impact: f64,
    pub path: Vec<Address>,
    pub source: DexSource,
    /// Raw calldata for execution (from aggregators)
    pub calldata: Option<Vec<u8>>,
    /// Target contract for execution
    pub to: Option<Address>,
}

/// 1inch API response structures
#[derive(Debug, Deserialize)]
struct OneInchQuoteResponse {
    #[serde(rename = "dstAmount")]
    dst_amount: String,
    #[serde(default)]
    protocols: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OneInchSwapResponse {
    #[serde(rename = "dstAmount")]
    dst_amount: String,
    tx: OneInchTx,
}

#[derive(Debug, Deserialize)]
struct OneInchTx {
    to: String,
    data: String,
    value: String,
}

/// Paraswap API response structures
#[derive(Debug, Deserialize)]
struct ParaswapPriceResponse {
    #[serde(rename = "priceRoute")]
    price_route: ParaswapPriceRoute,
}

#[derive(Debug, Deserialize)]
struct ParaswapPriceRoute {
    #[serde(rename = "destAmount")]
    dest_amount: String,
    #[serde(rename = "gasCostUSD")]
    gas_cost_usd: Option<String>,
}

/// Multi-DEX swap quoter
pub struct MultiDexQuoter {
    http_client: Client,
    /// Optional 1inch API key for higher rate limits
    oneinch_api_key: Option<String>,
}

impl MultiDexQuoter {
    pub fn new(oneinch_api_key: Option<String>) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        
        Self {
            http_client,
            oneinch_api_key,
        }
    }
    
    /// Get best quote across all DEX sources
    pub async fn get_best_quote(
        &self,
        provider: &Provider<Http>,
        chain: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> anyhow::Result<Option<SwapQuote>> {
        let chain_id = match get_chain_id(chain) {
            Some(id) => id,
            None => {
                warn!("Unknown chain: {}", chain);
                return Ok(None);
            }
        };
        
        // Try sources in priority order, return first successful
        // This is fast-fail - we don't wait for all sources
        
        // 1. Try 1inch first (best aggregation)
        match self.quote_1inch(chain_id, token_in, token_out, amount_in).await {
            Ok(Some(quote)) => {
                debug!("Got 1inch quote: {} out", quote.amount_out);
                return Ok(Some(quote));
            }
            Ok(None) => debug!("1inch: no quote"),
            Err(e) => debug!("1inch error: {}", e),
        }
        
        // 2. Try Paraswap
        match self.quote_paraswap(chain_id, token_in, token_out, amount_in).await {
            Ok(Some(quote)) => {
                debug!("Got Paraswap quote: {} out", quote.amount_out);
                return Ok(Some(quote));
            }
            Ok(None) => debug!("Paraswap: no quote"),
            Err(e) => debug!("Paraswap error: {}", e),
        }
        
        // 3. Fallback to Uniswap V3 on-chain
        match self.quote_uniswap(provider, chain, token_in, token_out, amount_in).await {
            Ok(Some(quote)) => {
                debug!("Got Uniswap quote: {} out", quote.amount_out);
                return Ok(Some(quote));
            }
            Ok(None) => debug!("Uniswap: no quote"),
            Err(e) => debug!("Uniswap error: {}", e),
        }
        
        Ok(None)
    }
    
    /// Get quotes from all sources and return the best one
    pub async fn get_best_quote_parallel(
        &self,
        provider: &Provider<Http>,
        chain: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> anyhow::Result<Option<SwapQuote>> {
        let chain_id = match get_chain_id(chain) {
            Some(id) => id,
            None => return Ok(None),
        };
        
        // Run all quotes in parallel
        let (oneinch, paraswap, uniswap) = tokio::join!(
            self.quote_1inch(chain_id, token_in, token_out, amount_in),
            self.quote_paraswap(chain_id, token_in, token_out, amount_in),
            self.quote_uniswap(provider, chain, token_in, token_out, amount_in),
        );
        
        // Collect successful quotes
        let mut quotes: Vec<SwapQuote> = Vec::new();
        
        if let Ok(Some(q)) = oneinch {
            quotes.push(q);
        }
        if let Ok(Some(q)) = paraswap {
            quotes.push(q);
        }
        if let Ok(Some(q)) = uniswap {
            quotes.push(q);
        }
        
        if quotes.is_empty() {
            return Ok(None);
        }
        
        // Find best quote (highest output)
        quotes.sort_by(|a, b| b.amount_out.cmp(&a.amount_out));
        
        let best = quotes.remove(0);
        info!(
            "Best quote from {}: {} -> {} (others: {})",
            best.source,
            amount_in,
            best.amount_out,
            quotes.iter().map(|q| format!("{}:{}", q.source, q.amount_out)).collect::<Vec<_>>().join(", ")
        );
        
        Ok(Some(best))
    }
    
    /// Quote from 1inch Fusion API
    async fn quote_1inch(
        &self,
        chain_id: u64,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> anyhow::Result<Option<SwapQuote>> {
        let url = format!(
            "https://api.1inch.dev/swap/v6.0/{}/quote?src={:?}&dst={:?}&amount={}",
            chain_id, token_in, token_out, amount_in
        );
        
        let mut request = self.http_client.get(&url);
        
        if let Some(ref api_key) = self.oneinch_api_key {
            request = request.header("Authorization", format!("Bearer {}", api_key));
        }
        
        let response = request.send().await?;
        
        if !response.status().is_success() {
            return Ok(None);
        }
        
        let quote: OneInchQuoteResponse = response.json().await?;
        let amount_out = U256::from_dec_str(&quote.dst_amount)?;
        
        if amount_out.is_zero() {
            return Ok(None);
        }
        
        Ok(Some(SwapQuote {
            token_in,
            token_out,
            amount_in,
            amount_out,
            fee: 0,
            price_impact: 0.0,
            path: vec![token_in, token_out],
            source: DexSource::OneInch,
            calldata: None,
            to: None,
        }))
    }
    
    /// Quote from Paraswap API
    async fn quote_paraswap(
        &self,
        chain_id: u64,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> anyhow::Result<Option<SwapQuote>> {
        let url = format!(
            "https://apiv5.paraswap.io/prices?srcToken={:?}&destToken={:?}&amount={}&srcDecimals=18&destDecimals=18&side=SELL&network={}",
            token_in, token_out, amount_in, chain_id
        );
        
        let response = self.http_client.get(&url).send().await?;
        
        if !response.status().is_success() {
            return Ok(None);
        }
        
        let price_response: ParaswapPriceResponse = response.json().await?;
        let amount_out = U256::from_dec_str(&price_response.price_route.dest_amount)?;
        
        if amount_out.is_zero() {
            return Ok(None);
        }
        
        Ok(Some(SwapQuote {
            token_in,
            token_out,
            amount_in,
            amount_out,
            fee: 0,
            price_impact: 0.0,
            path: vec![token_in, token_out],
            source: DexSource::Paraswap,
            calldata: None,
            to: None,
        }))
    }
    
    /// Quote from Uniswap V3 on-chain
    async fn quote_uniswap(
        &self,
        provider: &Provider<Http>,
        chain: &str,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> anyhow::Result<Option<SwapQuote>> {
        let quoter_addr = match get_quoter_address(chain) {
            Some(a) => a,
            None => return Ok(None),
        };
        
        let quoter = IQuoterV2::new(quoter_addr, Arc::new(provider.clone()));
        
        // Try different fee tiers
        let fees = [FEE_MEDIUM, FEE_LOW, FEE_HIGH, FEE_LOWEST];
        
        for fee in fees {
            match quoter
                .quote_exact_input_single(
                    token_in,
                    token_out,
                    fee.try_into().unwrap(),
                    amount_in,
                    U256::zero(),
                )
                .call()
                .await
            {
                Ok(amount_out) if !amount_out.is_zero() => {
                    return Ok(Some(SwapQuote {
                        token_in,
                        token_out,
                        amount_in,
                        amount_out,
                        fee,
                        price_impact: 0.0,
                        path: vec![token_in, token_out],
                        source: DexSource::UniswapV3,
                        calldata: None,
                        to: None,
                    }));
                }
                _ => continue,
            }
        }
        
        // Try multi-hop through intermediates
        let intermediates = get_intermediate_tokens(chain);
        
        for intermediate in intermediates {
            if intermediate == token_in || intermediate == token_out {
                continue;
            }
            
            // First hop
            let mut first_out = U256::zero();
            let mut first_fee = 0u32;
            
            for fee in [FEE_MEDIUM, FEE_LOW, FEE_HIGH] {
                if let Ok(out) = quoter
                    .quote_exact_input_single(
                        token_in,
                        intermediate,
                        fee.try_into().unwrap(),
                        amount_in,
                        U256::zero(),
                    )
                    .call()
                    .await
                {
                    if out > first_out {
                        first_out = out;
                        first_fee = fee;
                    }
                }
            }
            
            if first_out.is_zero() {
                continue;
            }
            
            // Second hop
            for fee in [FEE_MEDIUM, FEE_LOW, FEE_HIGH] {
                if let Ok(amount_out) = quoter
                    .quote_exact_input_single(
                        intermediate,
                        token_out,
                        fee.try_into().unwrap(),
                        first_out,
                        U256::zero(),
                    )
                    .call()
                    .await
                {
                    if !amount_out.is_zero() {
                        return Ok(Some(SwapQuote {
                            token_in,
                            token_out,
                            amount_in,
                            amount_out,
                            fee: first_fee.max(fee),
                            price_impact: 0.0,
                            path: vec![token_in, intermediate, token_out],
                            source: DexSource::UniswapV3,
                            calldata: None,
                            to: None,
                        }));
                    }
                }
            }
        }
        
        Ok(None)
    }
}

fn get_intermediate_tokens(chain: &str) -> Vec<Address> {
    match chain {
        "base" => vec![
            "0x4200000000000000000000000000000000000006".parse().unwrap(), // WETH
            "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".parse().unwrap(), // USDC
        ],
        "polygon" => vec![
            "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619".parse().unwrap(), // WETH
            "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270".parse().unwrap(), // WMATIC
            "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174".parse().unwrap(), // USDC
        ],
        "arbitrum" => vec![
            "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1".parse().unwrap(), // WETH
            "0xaf88d065e77c8cC2239327C5EDb3A432268e5831".parse().unwrap(), // USDC
        ],
        "avalanche" => vec![
            "0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7".parse().unwrap(), // WAVAX
            "0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E".parse().unwrap(), // USDC
        ],
        "bnb" => vec![
            "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c".parse().unwrap(), // WBNB
            "0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d".parse().unwrap(), // USDC
            "0x55d398326f99059fF775485246999027B3197955".parse().unwrap(), // USDT
        ],
        _ => vec![],
    }
}

// ============================================================================
// Legacy API compatibility
// ============================================================================

/// Simple quote function (uses sequential fallback)
pub async fn quote_swap(
    provider: &Provider<Http>,
    chain: &str,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> anyhow::Result<Option<SwapQuote>> {
    let quoter = MultiDexQuoter::new(None);
    quoter.get_best_quote(provider, chain, token_in, token_out, amount_in).await
}

/// Validate that a liquidation can be profitably executed
pub async fn validate_liquidation_swap(
    provider: &Provider<Http>,
    chain: &str,
    collateral_token: Address,
    debt_token: Address,
    collateral_amount: U256,
    debt_to_cover: U256,
    liquidation_bonus_bps: u64,
) -> anyhow::Result<Option<SwapQuote>> {
    let bonus_multiplier = 10000 + liquidation_bonus_bps;
    let expected_collateral = collateral_amount * bonus_multiplier / 10000;
    
    let quoter = MultiDexQuoter::new(None);
    let quote = quoter.get_best_quote(
        provider,
        chain,
        collateral_token,
        debt_token,
        expected_collateral,
    ).await?;
    
    match quote {
        Some(q) => {
            if q.amount_out >= debt_to_cover {
                debug!(
                    "Swap valid via {}: {} -> {} (need {})",
                    q.source, expected_collateral, q.amount_out, debt_to_cover
                );
                Ok(Some(q))
            } else {
                debug!(
                    "Swap insufficient via {}: {} -> {} (need {})",
                    q.source, expected_collateral, q.amount_out, debt_to_cover
                );
                Ok(None)
            }
        }
        None => {
            debug!("No swap path found across any DEX");
            Ok(None)
        }
    }
}

/// Validate with parallel quotes (slower but finds best rate)
pub async fn validate_liquidation_swap_best(
    provider: &Provider<Http>,
    chain: &str,
    collateral_token: Address,
    debt_token: Address,
    collateral_amount: U256,
    debt_to_cover: U256,
    liquidation_bonus_bps: u64,
) -> anyhow::Result<Option<SwapQuote>> {
    let bonus_multiplier = 10000 + liquidation_bonus_bps;
    let expected_collateral = collateral_amount * bonus_multiplier / 10000;
    
    let quoter = MultiDexQuoter::new(None);
    let quote = quoter.get_best_quote_parallel(
        provider,
        chain,
        collateral_token,
        debt_token,
        expected_collateral,
    ).await?;
    
    match quote {
        Some(q) if q.amount_out >= debt_to_cover => Ok(Some(q)),
        _ => Ok(None),
    }
}
