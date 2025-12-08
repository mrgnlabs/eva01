//! Titan swap functionality
//!
//! This module provides a simple interface for performing swaps on Titan protocol.

use anyhow::{anyhow, Result};
use atlas::titan::{transaction_builder, TitanClient};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};

/// Perform a token swap on Titan protocol
///
/// # Arguments
/// * `input_token` - Input token mint address (base58 string)
/// * `output_token` - Output token mint address (base58 string)
/// * `amount_lamports` - Amount to swap in lamports (smallest unit of the input token)
/// * `slippage_bps` - Slippage tolerance in basis points (e.g., 25 = 0.25%)
///
/// # Returns
/// Transaction signature (tx_id) as a string
///
/// # Environment Variables
/// * `RPC_URL` - Solana RPC endpoint (required)
/// * `TITAN_API_KEY` - Titan API key (optional if HERMES_ENDPOINT is set)
/// * `HERMES_ENDPOINT` - Hermes proxy endpoint (optional, enables proxy mode)
/// * `TITAN_WS_ENDPOINT` - Titan WebSocket endpoint (optional, defaults to "us1.api.demo.titan.exchange")
/// * `WALLET_KEYPAIR` - Wallet keypair as JSON array (required)
/// * `SOLANA_KEYPAIR` - Alternative env var for wallet keypair (optional)
pub async fn swap_tokens(
    input_token: &str,
    output_token: &str,
    amount_lamports: u64,
    slippage_bps: u16,
) -> Result<String> {
    // Load configuration from environment
    let rpc_url =
        std::env::var("RPC_URL").map_err(|_| anyhow!("RPC_URL environment variable is not set"))?;

    // Load keypair
    let wallet_keypair_env = std::env::var("WALLET_KEYPAIR")
        .or_else(|_| std::env::var("SOLANA_KEYPAIR"))
        .map_err(|_| anyhow!("WALLET_KEYPAIR or SOLANA_KEYPAIR environment variable is not set"))?;

    let wallet_keypair_bytes: Vec<u8> = serde_json::from_str(&wallet_keypair_env)
        .map_err(|e| anyhow!("Invalid WALLET_KEYPAIR JSON format: {}", e))?;

    let keypair = Keypair::from_bytes(&wallet_keypair_bytes)
        .map_err(|e| anyhow!("Invalid keypair bytes: {}", e))?;

    let user_pubkey = keypair.pubkey().to_string();

    // Determine Titan endpoint configuration
    let (titan_ws_endpoint, titan_api_key) = if let Ok(api_key) = std::env::var("TITAN_API_KEY") {
        // Direct mode with API key
        let endpoint = std::env::var("TITAN_WS_ENDPOINT")
            .unwrap_or_else(|_| "us1.api.demo.titan.exchange".to_string());

        if api_key.is_empty() {
            return Err(anyhow!(
                "TITAN_API_KEY environment variable is set but empty"
            ));
        }

        (endpoint, api_key)
    } else if let Ok(hermes_endpoint) = std::env::var("HERMES_ENDPOINT") {
        let ws_endpoint = format!("{}/ws", hermes_endpoint);
        (ws_endpoint, String::new())
    } else {
        return Err(anyhow!(
            "Either TITAN_API_KEY or HERMES_ENDPOINT environment variable must be set"
        ));
    };

    // Create async RPC client
    let rpc_client = RpcClient::new(rpc_url.clone());

    // Create Titan client and connect
    let titan_client = TitanClient::new(titan_ws_endpoint.clone(), titan_api_key.clone());
    titan_client
        .connect()
        .await
        .map_err(|e| anyhow!("Failed to connect to Titan: {}", e))?;

    // Request swap quotes with slippage
    let (_provider, route) = titan_client
        .request_swap_quotes(
            input_token,
            output_token,
            amount_lamports,
            &user_pubkey,
            Some(slippage_bps),
        )
        .await
        .map_err(|e| anyhow!("Failed to get swap quotes: {}", e))?;

    // Clean up Titan connection
    let _ = titan_client.close().await;

    // Get recent blockhash
    let recent_blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .map_err(|e| anyhow!("Failed to get blockhash: {}", e))?;

    // Build transaction
    let transaction_bytes = transaction_builder::build_transaction_from_route(
        &route,
        keypair.pubkey(),
        recent_blockhash,
        &rpc_url,
        None, // No Jito tip for now
    )
    .await
    .map_err(|e| anyhow!("Failed to build transaction: {}", e))?;

    // Deserialize transaction
    let mut transaction: VersionedTransaction = bincode::deserialize(&transaction_bytes)
        .map_err(|e| anyhow!("Failed to deserialize transaction: {}", e))?;

    // Sign transaction
    let message = transaction.message.serialize();
    let signature = keypair.sign_message(&message);
    transaction.signatures[0] = signature;

    // Send transaction
    let tx_signature = rpc_client
        .send_transaction(&transaction)
        .await
        .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

    Ok(tx_signature.to_string())
}

#[cfg(test)]
mod swap_test;
