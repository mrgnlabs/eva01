//! Test for Titan swap functionality

use crate::titan::swap_tokens;
use anyhow::Result;

/// Test that performs a swap using environment variables
///
/// Environment variables:
/// - `TITAN_INPUT_TOKEN`: Input token mint (default: SOL)
/// - `TITAN_OUTPUT_TOKEN`: Output token mint (default: USDC)
/// - `TITAN_AMOUNT`: Amount in lamports (default: 0.01 SOL = 10_000_000)
/// - `TITAN_SLIPPAGE_BPS`: Slippage in basis points (default: 25)
/// - `RPC_URL`: Solana RPC endpoint (required)
/// - `TITAN_API_KEY` or `HERMES_ENDPOINT`: Titan connection (required)
/// - `WALLET_KEYPAIR` or `SOLANA_KEYPAIR`: Wallet keypair JSON (required)
#[tokio::test]
#[ignore] // Ignore by default - requires environment setup
async fn test_swap_tokens() -> Result<()> {
    // Read input token (default: SOL)
    let input_token = std::env::var("TITAN_INPUT_TOKEN")
        .unwrap_or_else(|_| "So11111111111111111111111111111111111111112".to_string());

    // Read output token (default: USDC)
    let output_token = std::env::var("TITAN_OUTPUT_TOKEN")
        .unwrap_or_else(|_| "USDSwr9ApdHk5bvJKMjzff41FfuX8bSxdKcR81vTwcA".to_string());

    // Read amount in lamports (default: 0.01 SOL = 10_000_000 lamports)
    let amount_lamports: u64 = std::env::var("TITAN_AMOUNT")
        .unwrap_or_else(|_| "10000000".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid TITAN_AMOUNT: {}", e))?;

    // Read slippage in basis points (default: 25 = 0.25%)
    let slippage_bps: u16 = std::env::var("TITAN_SLIPPAGE_BPS")
        .unwrap_or_else(|_| "25".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid TITAN_SLIPPAGE_BPS: {}", e))?;

    // Log input parameters
    println!("Swap Input:");
    println!("  Input Token: {}", input_token);
    println!("  Output Token: {}", output_token);
    println!("  Amount: {} lamports", amount_lamports);
    println!("  Slippage: {} bps", slippage_bps);

    let tx_id = swap_tokens(&input_token, &output_token, amount_lamports, slippage_bps).await?;

    // Validate that we got a transaction ID
    assert!(!tx_id.is_empty(), "Transaction ID should not be empty");

    // Log output
    println!("Swap Output:");
    println!("  Transaction ID: {}", tx_id);

    Ok(())
}
