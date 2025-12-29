//! Transaction execution for liquidations with Flashbots MEV protection.

use ethers::prelude::*;
use ethers::providers::{Provider, Http, Middleware};
use ethers::signers::Signer;
use ethers::types::{Address, U256, Bytes, TransactionRequest, H256};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::utils::keccak256;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn, error};

use crate::types::{Position, Protocol};
use crate::chains::ChainState;

/// Flashbots RPC endpoints by chain
pub fn get_flashbots_rpc(chain: &str) -> Option<&'static str> {
    match chain {
        "ethereum" => Some("https://relay.flashbots.net"),
        "base" => Some("https://rpc.flashbots.net/fast"),
        _ => None,
    }
}

/// Flashbots Protect RPC (simpler, no bundle signing needed)
pub fn get_flashbots_protect_rpc(chain: &str) -> Option<&'static str> {
    match chain {
        "ethereum" => Some("https://rpc.flashbots.net"),
        "base" => Some("https://rpc.flashbots.net/fast"),
        _ => None,
    }
}

// Liquidator contract ABI for flash loan liquidations
// Matches deployed contract at 0x163A862679E73329eA835aC302E54aCBee7A58B1
abigen!(
    IFlashLiquidator,
    r#"[
        function executeLiquidation(address user, address collateralAsset, address debtAsset, uint256 debtToCover) external
        function withdrawToken(address token) external
        function withdrawETH() external
        function owner() external view returns (address)
        function POOL() external view returns (address)
        function ROUTER() external view returns (address)
    ]"#
);

/// Flashbots bundle request
#[derive(Debug, Serialize)]
struct FlashbotsBundle {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: Vec<FlashbotsBundleParams>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlashbotsBundleParams {
    txs: Vec<String>,           // Signed transaction hex strings
    block_number: String,       // Target block (hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    min_timestamp: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_timestamp: Option<u64>,
}

/// Flashbots bundle response
#[derive(Debug, Deserialize)]
struct FlashbotsResponse {
    #[serde(default)]
    result: Option<FlashbotsResult>,
    #[serde(default)]
    error: Option<FlashbotsError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlashbotsResult {
    bundle_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FlashbotsError {
    message: String,
    #[serde(default)]
    code: i64,
}

pub struct Executor {
    pub dry_run: bool,
    pub min_profit_usd: f64,
    pub mev_threshold_usd: f64,
    http_client: Client,
}

impl Executor {
    pub fn new(dry_run: bool, min_profit_usd: f64, mev_threshold_usd: f64) -> Self {
        Self {
            dry_run,
            min_profit_usd,
            mev_threshold_usd,
            http_client: Client::new(),
        }
    }
    
    /// Check if we should use MEV protection
    pub fn should_use_mev(&self, debt_usd: f64, chain: &str) -> bool {
        debt_usd >= self.mev_threshold_usd && get_flashbots_protect_rpc(chain).is_some()
    }
    
    /// Execute an Aave liquidation via flash loan
    pub async fn execute_aave_liquidation(
        &self,
        chain: &ChainState,
        position: &Position,
        collateral_asset: Address,
        debt_asset: Address,
        debt_to_cover: U256,
    ) -> anyhow::Result<Option<TxHash>> {
        if self.dry_run {
            info!("🧪 DRY RUN: Would execute Aave liquidation");
            info!("   User: {:?}", position.user);
            info!("   Collateral: {:?}", collateral_asset);
            info!("   Debt: {:?}", debt_asset);
            info!("   Amount: {}", debt_to_cover);
            info!("   MEV Protection: {}", self.should_use_mev(position.debt_usd, &chain.config.name));
            return Ok(None);
        }
        
        let liquidator_address: Address = match &chain.config.liquidator_address {
            Some(addr) => addr.parse()?,
            None => {
                warn!("No liquidator contract configured for {}", chain.config.name);
                return Ok(None);
            }
        };
        
        // Build transaction
        let tx_data = build_liquidation_calldata(
            position.user,
            collateral_asset,
            debt_asset,
            debt_to_cover,
            3000, // 0.3% pool fee
        );
        
        let estimate_tx: TypedTransaction = TransactionRequest::new()
            .to(liquidator_address)
            .data(tx_data.clone())
            .from(chain.wallet.address())
            .into();
        
        let gas_estimate = chain.provider()
            .estimate_gas(&estimate_tx, None)
            .await
            .unwrap_or(U256::from(800_000));
        
        let gas_limit = gas_estimate * 120 / 100; // 20% buffer
        let gas_price = chain.provider().get_gas_price().await?;
        let nonce = chain.next_nonce();
        
        let tx = TransactionRequest::new()
            .to(liquidator_address)
            .data(tx_data)
            .gas(gas_limit)
            .gas_price(gas_price)
            .nonce(nonce)
            .chain_id(chain.config.chain_id);
        
        // Use Flashbots Protect for MEV protection on supported chains
        if self.should_use_mev(position.debt_usd, &chain.config.name) {
            info!("🛡️  Using Flashbots Protect for MEV protection");
            return self.send_via_flashbots_protect(chain, tx).await;
        }
        
        // Standard submission
        info!("📤 Sending liquidation TX (standard)...");
        self.send_standard(chain, tx).await
    }
    
    /// Send transaction via Flashbots Protect RPC
    /// This is the simplest MEV protection - just send to a different RPC
    async fn send_via_flashbots_protect(
        &self,
        chain: &ChainState,
        tx: TransactionRequest,
    ) -> anyhow::Result<Option<TxHash>> {
        let flashbots_rpc = match get_flashbots_protect_rpc(&chain.config.name) {
            Some(rpc) => rpc,
            None => {
                warn!("No Flashbots RPC for {}, falling back to standard", chain.config.name);
                return self.send_standard(chain, tx).await;
            }
        };
        
        // Create a provider pointing to Flashbots
        let flashbots_provider = Provider::<Http>::try_from(flashbots_rpc)?;
        
        // Sign the transaction
        let typed_tx: TypedTransaction = tx.clone().into();
        let signature = chain.wallet.sign_transaction(&typed_tx).await?;
        let signed_tx = tx.rlp_signed(&signature);
        
        info!("🛡️  Submitting to Flashbots Protect: {}", flashbots_rpc);
        
        // Send raw transaction to Flashbots
        match flashbots_provider.send_raw_transaction(signed_tx).await {
            Ok(pending_tx) => {
                let tx_hash = pending_tx.tx_hash();
                info!("⏳ TX submitted via Flashbots: {:?}", tx_hash);
                
                // Wait for confirmation (Flashbots may take longer)
                match tokio::time::timeout(
                    std::time::Duration::from_secs(120), // Longer timeout for Flashbots
                    pending_tx,
                ).await {
                    Ok(Ok(Some(receipt))) => {
                        if receipt.status == Some(U64::from(1)) {
                            info!("✅ Flashbots TX successful! Gas: {}", receipt.gas_used.unwrap_or_default());
                            return Ok(Some(tx_hash));
                        } else {
                            error!("❌ Flashbots TX reverted");
                            return Ok(None);
                        }
                    }
                    Ok(Ok(None)) => {
                        warn!("⏰ Flashbots TX pending (no receipt yet)");
                        return Ok(Some(tx_hash));
                    }
                    Ok(Err(e)) => {
                        error!("❌ Flashbots TX error: {}", e);
                    }
                    Err(_) => {
                        warn!("⏰ Flashbots TX timeout - may still land");
                        return Ok(Some(tx_hash));
                    }
                }
            }
            Err(e) => {
                error!("❌ Flashbots submission failed: {}", e);
                // Fallback to standard
                warn!("Falling back to standard submission");
                return self.send_standard(chain, tx).await;
            }
        }
        
        Ok(None)
    }
    
    /// Send transaction via Flashbots Bundle API (advanced)
    /// This allows targeting specific blocks and provides better guarantees
    #[allow(dead_code)]
    async fn send_via_flashbots_bundle(
        &self,
        chain: &ChainState,
        tx: TransactionRequest,
    ) -> anyhow::Result<Option<TxHash>> {
        let flashbots_rpc = match get_flashbots_rpc(&chain.config.name) {
            Some(rpc) => rpc,
            None => return self.send_standard(chain, tx).await,
        };
        
        // Sign the transaction
        let typed_tx: TypedTransaction = tx.clone().into();
        let signature = chain.wallet.sign_transaction(&typed_tx).await?;
        let signed_tx = tx.rlp_signed(&signature);
        let signed_tx_hex = format!("0x{}", hex::encode(&signed_tx));
        
        // Get current block and target next block
        let current_block = chain.provider().get_block_number().await?;
        let target_block = current_block + 1;
        let target_block_hex = format!("0x{:x}", target_block);
        
        // Build bundle request
        let bundle = FlashbotsBundle {
            jsonrpc: "2.0",
            id: 1,
            method: "eth_sendBundle",
            params: vec![FlashbotsBundleParams {
                txs: vec![signed_tx_hex],
                block_number: target_block_hex,
                min_timestamp: None,
                max_timestamp: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs() + 120
                ),
            }],
        };
        
        // Sign the bundle payload for authentication
        let body = serde_json::to_string(&bundle)?;
        let body_hash = keccak256(body.as_bytes());
        let fb_signature = chain.wallet.sign_message(&body_hash).await?;
        let fb_auth_header = format!(
            "{}:0x{}",
            chain.wallet.address(),
            hex::encode(fb_signature.to_vec())
        );
        
        info!("🛡️  Submitting Flashbots bundle for block {}", target_block);
        
        // Send bundle
        let response = self.http_client
            .post(flashbots_rpc)
            .header("Content-Type", "application/json")
            .header("X-Flashbots-Signature", fb_auth_header)
            .body(body)
            .send()
            .await?;
        
        let fb_response: FlashbotsResponse = response.json().await?;
        
        if let Some(error) = fb_response.error {
            error!("❌ Flashbots bundle error: {} (code: {})", error.message, error.code);
            return Ok(None);
        }
        
        if let Some(result) = fb_response.result {
            if let Some(bundle_hash) = result.bundle_hash {
                info!("📦 Bundle submitted: {}", bundle_hash);
                
                // Calculate expected TX hash
                let tx_hash = H256::from_slice(&keccak256(&signed_tx));
                
                // Wait for inclusion (check multiple blocks)
                for _ in 0..12 {
                    tokio::time::sleep(std::time::Duration::from_secs(12)).await;
                    
                    if let Ok(Some(receipt)) = chain.provider().get_transaction_receipt(tx_hash).await {
                        if receipt.status == Some(U64::from(1)) {
                            info!("✅ Bundle landed! TX: {:?}", tx_hash);
                            return Ok(Some(tx_hash));
                        } else {
                            error!("❌ Bundle TX reverted");
                            return Ok(None);
                        }
                    }
                }
                
                warn!("⏰ Bundle may not have landed after 12 blocks");
                return Ok(Some(tx_hash));
            }
        }
        
        Ok(None)
    }
    
    /// Standard transaction submission
    async fn send_standard(
        &self,
        chain: &ChainState,
        tx: TransactionRequest,
    ) -> anyhow::Result<Option<TxHash>> {
        // Sign and send
        let typed_tx: TypedTransaction = tx.clone().into();
        let signature = chain.wallet.sign_transaction(&typed_tx).await?;
        let signed_tx = tx.rlp_signed(&signature);
        
        info!("📤 Sending TX (standard)...");
        
        let pending_tx = chain.provider().send_raw_transaction(signed_tx).await?;
        let tx_hash = pending_tx.tx_hash();
        
        info!("⏳ TX submitted: {:?}", tx_hash);
        
        match tokio::time::timeout(
            std::time::Duration::from_secs(60),
            pending_tx,
        ).await {
            Ok(Ok(Some(receipt))) => {
                if receipt.status == Some(U64::from(1)) {
                    info!("✅ TX successful! Gas: {}", receipt.gas_used.unwrap_or_default());
                    return Ok(Some(tx_hash));
                } else {
                    error!("❌ TX reverted");
                    return Ok(None);
                }
            }
            Ok(Ok(None)) => {
                warn!("⏰ TX pending (no receipt)");
                return Ok(Some(tx_hash));
            }
            Ok(Err(e)) => {
                error!("❌ TX failed: {}", e);
            }
            Err(_) => {
                warn!("⏰ TX timeout - may still succeed");
                return Ok(Some(tx_hash));
            }
        }
        
        Ok(None)
    }
    
    /// Estimate gas cost in USD
    pub async fn estimate_gas_cost_usd(
        &self,
        chain: &ChainState,
        gas_limit: u64,
    ) -> anyhow::Result<f64> {
        let gas_price = chain.provider().get_gas_price().await?;
        let gas_price_gwei = gas_price.as_u128() as f64 / 1e9;
        
        let gas_cost_native = gas_limit as f64 * gas_price_gwei * 1e-9;
        let gas_cost_usd = gas_cost_native * chain.config.native_price_fallback;
        
        Ok(gas_cost_usd)
    }
    
    /// Calculate optimal debt to cover (close factor)
    pub fn calculate_debt_to_cover(
        &self,
        protocol: Protocol,
        total_debt: U256,
    ) -> U256 {
        let close_factor = match protocol {
            Protocol::Aave => 50,
            Protocol::Compound => 50,
            Protocol::Venus => 50,
        };
        
        total_debt * close_factor / 100
    }
}

/// Build liquidation calldata for deployed contract
/// Function: executeLiquidation(address user, address collateralAsset, address debtAsset, uint256 debtToCover)
/// Selector: 0x05c3786d
pub fn build_liquidation_calldata(
    user: Address,
    collateral_asset: Address,
    debt_asset: Address,
    debt_to_cover: U256,
    _pool_fee: u32, // Unused - kept for API compatibility
) -> Bytes {
    // executeLiquidation(address,address,address,uint256) = 0x05c3786d
    let selector = ethers::utils::id("executeLiquidation(address,address,address,uint256)");
    
    let mut data = selector[0..4].to_vec();
    
    data.extend_from_slice(&ethers::abi::encode(&[
        ethers::abi::Token::Address(user),
        ethers::abi::Token::Address(collateral_asset),
        ethers::abi::Token::Address(debt_asset),
        ethers::abi::Token::Uint(debt_to_cover),
    ]));
    
    Bytes::from(data)
}

/// Pre-computed transaction template for faster execution
#[derive(Clone)]
pub struct TxTemplate {
    pub to: Address,
    pub data: Bytes,
    pub gas_limit: U256,
    pub value: U256,
}

impl TxTemplate {
    pub fn new(
        liquidator_address: Address,
        user: Address,
        collateral_asset: Address,
        debt_asset: Address,
        debt_to_cover: U256,
        pool_fee: u32,
    ) -> Self {
        Self {
            to: liquidator_address,
            data: build_liquidation_calldata(user, collateral_asset, debt_asset, debt_to_cover, pool_fee),
            gas_limit: U256::from(800_000),
            value: U256::zero(),
        }
    }
    
    pub fn to_request(&self, nonce: u64, gas_price: U256, chain_id: u64) -> TransactionRequest {
        TransactionRequest::new()
            .to(self.to)
            .data(self.data.clone())
            .gas(self.gas_limit)
            .gas_price(gas_price)
            .nonce(nonce)
            .value(self.value)
            .chain_id(chain_id)
    }
}
