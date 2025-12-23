//! Titan swap functionality
//!
//! This module provides a simple interface for performing swaps on Titan protocol.

use crate::utils::accessor;
use anyhow::{anyhow, Result};
use atlas::titan::{transaction_builder, types::SwapRoute, TitanClient};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use spl_associated_token_account::get_associated_token_address;
use std::str::FromStr;

/// Configuration for Titan swap client
#[derive(Clone, Debug)]
pub struct TitanSwapConfig {
    /// Solana RPC endpoint URL
    pub rpc_url: String,
    /// Titan WebSocket endpoint (e.g., "us1.api.demo.titan.exchange")
    pub titan_ws_endpoint: String,
    /// Titan API key (required for direct mode)
    pub titan_api_key: Option<String>,
    /// Hermes proxy endpoint (alternative to direct mode)
    pub hermes_endpoint: Option<String>,
    /// Wallet keypair bytes
    pub wallet_keypair: Vec<u8>,
}

/// Titan swap client that maintains a persistent WebSocket connection
///
/// The WebSocket connection is kept open between swaps for efficiency.
/// Each swap creates a new quote stream, gets quotes, then stops the stream.
/// You won't receive spam - streams are explicitly managed per swap.
pub struct TitanSwapClient {
    /// Titan WebSocket client (keeps connection open)
    titan_client: TitanClient,
    /// Solana RPC client
    rpc_client: RpcClient,
    /// Wallet keypair for signing transactions
    keypair: Keypair,
    /// RPC URL for transaction building
    rpc_url: String,
}

impl TitanSwapClient {
    /// Create a new Titan swap client and connect to Titan
    ///
    /// # Arguments
    /// * `config` - Configuration containing RPC URL, Titan endpoint, API key, and wallet keypair
    ///
    /// # Returns
    /// Connected TitanSwapClient ready to perform swaps
    pub async fn new(config: TitanSwapConfig) -> Result<Self> {
        // Validate keypair
        let keypair = Keypair::from_bytes(&config.wallet_keypair)
            .map_err(|e| anyhow!("Invalid keypair bytes: {}", e))?;

        // Determine Titan endpoint configuration
        let (titan_ws_endpoint, titan_api_key) = if let Some(api_key) = config.titan_api_key {
            if api_key.is_empty() {
                return Err(anyhow!("TITAN_API_KEY is set but empty"));
            }
            (config.titan_ws_endpoint, api_key)
        } else if let Some(hermes_endpoint) = config.hermes_endpoint {
            let ws_endpoint = format!("{}/ws", hermes_endpoint);
            (ws_endpoint, String::new())
        } else {
            return Err(anyhow!(
                "Either titan_api_key or hermes_endpoint must be provided"
            ));
        };

        // Create async RPC client
        let rpc_client = RpcClient::new(config.rpc_url.clone());

        // Create Titan client and connect
        let titan_client = TitanClient::new(titan_ws_endpoint, titan_api_key);
        titan_client
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to Titan: {}", e))?;

        Ok(Self {
            titan_client,
            rpc_client,
            keypair,
            rpc_url: config.rpc_url,
        })
    }

    /// Perform a token swap on Titan protocol
    ///
    /// # Arguments
    /// * `input_token` - Input token mint address (base58 string)
    /// * `output_token` - Output token mint address (base58 string)
    /// * `amount_lamports` - Amount to swap in lamports (smallest unit of the input token)
    /// * `slippage_bps` - Slippage tolerance in basis points (e.g., 25 = 0.25%)
    ///
    /// # Returns
    /// Tuple of (transaction_signature, swap_route)
    ///
    /// The swap route contains detailed information about the swap including:
    /// - Input and output amounts
    /// - Actual slippage (may differ from requested slippage)
    /// - Route steps and instructions
    /// - Platform fees
    ///
    /// # Note
    /// The WebSocket connection is reused for each swap. Each swap creates a new quote stream,
    /// receives quotes, then stops the stream. You won't receive unwanted messages.
    pub async fn swap(
        &self,
        input_token: &str,
        output_token: &str,
        amount_lamports: u64,
        slippage_bps: u16,
    ) -> Result<(String, SwapRoute)> {
        let user_pubkey = self.keypair.pubkey().to_string();

        // Request swap quotes with slippage
        // This creates a new stream, gets quotes, then stops the stream
        let (_provider, route) = self
            .titan_client
            .request_swap_quotes(
                input_token,
                output_token,
                amount_lamports,
                &user_pubkey,
                Some(slippage_bps),
            )
            .await
            .map_err(|e| anyhow!("Failed to get swap quotes: {}", e))?;

        // Get recent blockhash
        let recent_blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|e| anyhow!("Failed to get blockhash: {}", e))?;

        // Build transaction
        let transaction_bytes = transaction_builder::build_transaction_from_route(
            &route,
            self.keypair.pubkey(),
            recent_blockhash,
            &self.rpc_url,
            None, // No Jito tip for now
        )
        .await
        .map_err(|e| anyhow!("Failed to build transaction: {}", e))?;

        // Deserialize transaction
        let mut transaction: VersionedTransaction = bincode::deserialize(&transaction_bytes)
            .map_err(|e| anyhow!("Failed to deserialize transaction: {}", e))?;

        // Sign transaction
        let message = transaction.message.serialize();
        let signature = self.keypair.sign_message(&message);
        transaction.signatures[0] = signature;

        // Send transaction
        let tx_signature = self
            .rpc_client
            .send_transaction(&transaction)
            .await
            .map_err(|e| anyhow!("Failed to send transaction: {}", e))?;

        Ok((tx_signature.to_string(), route))
    }

    /// Get the actual token account balance for a given mint
    ///
    /// This is useful when you need to know the exact amount available after a swap,
    /// as the quoted amount may differ slightly from the actual received amount.
    pub async fn get_token_balance(&self, mint: &str) -> Result<u64> {
        let mint_pubkey =
            Pubkey::from_str(mint).map_err(|e| anyhow!("Invalid mint address: {}", e))?;

        let token_account = get_associated_token_address(&self.keypair.pubkey(), &mint_pubkey);

        let account_data = self
            .rpc_client
            .get_account_data(&token_account)
            .await
            .map_err(|e| anyhow!("Failed to get token account: {}", e))?;

        accessor::amount(&account_data)
            .map_err(|e| anyhow!("Failed to parse token account amount: {}", e))
    }

    /// Close the WebSocket connection
    ///
    /// Call this when you're done with the client to clean up resources.
    pub async fn close(&self) -> Result<()> {
        self.titan_client
            .close()
            .await
            .map_err(|e| anyhow!("Failed to close Titan connection: {}", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod swap_test;
