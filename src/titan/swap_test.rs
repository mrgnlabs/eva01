//! Test for Titan swap functionality

use crate::titan::{TitanSwapClient, TitanSwapConfig};
use anyhow::Result;

/// Test that performs a round-trip swap: SOL -> USDC -> SOL
///
/// This test performs two swaps:
/// 1. SOL -> USDC (using the specified input amount)
/// 2. USDC -> SOL (using the exact output amount from swap 1)
///
/// The WebSocket connection is reused for both swaps, demonstrating efficient connection management.
///
/// Environment variables:
/// - `TITAN_INPUT_TOKEN`: Input token mint (default: SOL)
/// - `TITAN_AMOUNT`: Amount in lamports (default: 0.01 SOL = 10_000_000)
/// - `TITAN_SLIPPAGE_BPS`: Slippage in basis points (default: 25)
/// - `RPC_URL`: Solana RPC endpoint (required)
/// - `TITAN_API_KEY` or `HERMES_ENDPOINT`: Titan connection (required)
/// - `TITAN_WS_ENDPOINT`: Titan WebSocket endpoint (optional, defaults to "us1.api.demo.titan.exchange")
/// - `WALLET_KEYPAIR` or `SOLANA_KEYPAIR`: Wallet keypair JSON (required)
#[tokio::test]
#[ignore] // Ignore by default - requires environment setup
async fn test_swap_tokens() -> Result<()> {
    // Read configuration from environment
    let rpc_url = std::env::var("RPC_URL")
        .map_err(|_| anyhow::anyhow!("RPC_URL environment variable is not set"))?;

    let wallet_keypair_env = std::env::var("WALLET_KEYPAIR")
        .or_else(|_| std::env::var("SOLANA_KEYPAIR"))
        .map_err(|_| {
            anyhow::anyhow!("WALLET_KEYPAIR or SOLANA_KEYPAIR environment variable is not set")
        })?;

    let wallet_keypair_bytes: Vec<u8> = serde_json::from_str(&wallet_keypair_env)
        .map_err(|e| anyhow::anyhow!("Invalid WALLET_KEYPAIR JSON format: {}", e))?;

    let titan_ws_endpoint = std::env::var("TITAN_WS_ENDPOINT")
        .unwrap_or_else(|_| "us1.api.demo.titan.exchange".to_string());

    let titan_api_key = std::env::var("TITAN_API_KEY").ok();
    let hermes_endpoint = std::env::var("HERMES_ENDPOINT").ok();

    // Create config
    let config = TitanSwapConfig {
        rpc_url,
        titan_ws_endpoint,
        titan_api_key,
        hermes_endpoint,
        wallet_keypair: wallet_keypair_bytes,
    };

    // Create client and connect
    let client = TitanSwapClient::new(config).await?;

    // Read swap parameters
    let amount_lamports: u64 = std::env::var("TITAN_AMOUNT")
        .unwrap_or_else(|_| "10000000".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid TITAN_AMOUNT: {}", e))?;

    let slippage_bps: u16 = std::env::var("TITAN_SLIPPAGE_BPS")
        .unwrap_or_else(|_| "25".to_string())
        .parse()
        .map_err(|e| anyhow::anyhow!("Invalid TITAN_SLIPPAGE_BPS: {}", e))?;

    // Token addresses
    let sol_token = "So11111111111111111111111111111111111111112";
    let usdc_token = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    // Swap 1: SOL -> USDC
    println!("Swap 1: SOL -> USDC");
    println!("  Input Token: {}", sol_token);
    println!("  Output Token: {}", usdc_token);
    println!("  Input Amount: {} lamports", amount_lamports);
    println!(
        "  Max Slippage: {} bps ({:.2}%)",
        slippage_bps,
        slippage_bps as f64 / 100.0
    );

    let (tx_id_usdc, route_usdc) = client
        .swap(sol_token, usdc_token, amount_lamports, slippage_bps)
        .await?;

    assert!(
        !tx_id_usdc.is_empty(),
        "USDC transaction ID should not be empty"
    );
    println!("  Transaction ID: {}", tx_id_usdc);
    println!("  Route Summary:");
    println!("    Input Amount: {} lamports", route_usdc.in_amount);
    println!(
        "    Quoted Output Amount: {} lamports",
        route_usdc.out_amount
    );
    println!(
        "    Actual Slippage: {} bps ({:.2}%)",
        route_usdc.slippage_bps,
        route_usdc.slippage_bps as f64 / 100.0
    );
    println!("    Route Steps: {}", route_usdc.steps.len());

    // Calculate total fees from steps
    let total_step_fees: u64 = route_usdc
        .steps
        .iter()
        .filter_map(|step| step.fee_amount)
        .sum();
    if total_step_fees > 0 {
        println!("    Total Step Fees: {} lamports", total_step_fees);
    }

    // Print platform fee if present
    if let Some(ref platform_fee) = route_usdc.platform_fee {
        println!(
            "    Platform Fee: {} lamports ({} bps)",
            platform_fee.amount, platform_fee.fee_bps
        );
    }

    // Print compute units if present
    if let Some(compute_units) = route_usdc.compute_units {
        println!("    Compute Units: {}", compute_units);
    }

    // Calculate expected net output (output - platform fee - step fees)
    let expected_net = route_usdc
        .out_amount
        .saturating_sub(
            route_usdc
                .platform_fee
                .as_ref()
                .map(|f| f.amount)
                .unwrap_or(0),
        )
        .saturating_sub(total_step_fees);
    if expected_net < route_usdc.out_amount {
        println!(
            "    Expected Net Output (after fees): {} lamports",
            expected_net
        );
    }

    if route_usdc.slippage_bps > slippage_bps {
        println!(
            "  ⚠️  Warning: Actual slippage ({}) exceeds max slippage ({})",
            route_usdc.slippage_bps, slippage_bps
        );
    }

    // Wait for transaction to settle and get actual balance
    println!("  Waiting for transaction to settle...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let actual_usdc_balance = client.get_token_balance(usdc_token).await?;
    println!("  Actual USDC Balance: {} lamports", actual_usdc_balance);

    if actual_usdc_balance < route_usdc.out_amount {
        let difference = route_usdc.out_amount - actual_usdc_balance;
        let difference_pct = (difference as f64 / route_usdc.out_amount as f64) * 100.0;
        println!(
            "  ⚠️  Note: Actual balance ({}) is {} lamports ({:.2}%) less than quoted amount ({})",
            actual_usdc_balance, difference, difference_pct, route_usdc.out_amount
        );
    }
    println!();

    // Swap 2: USDC -> SOL (using actual balance from swap 1)
    println!("Swap 2: USDC -> SOL");
    println!("  Input Token: {}", usdc_token);
    println!("  Output Token: {}", sol_token);
    println!(
        "  Input Amount: {} USDC lamports (actual balance from swap 1)",
        actual_usdc_balance
    );
    println!(
        "  Max Slippage: {} bps ({:.2}%)",
        slippage_bps,
        slippage_bps as f64 / 100.0
    );

    let (tx_id_sol, route_sol) = client
        .swap(usdc_token, sol_token, actual_usdc_balance, slippage_bps)
        .await?;

    assert!(
        !tx_id_sol.is_empty(),
        "SOL transaction ID should not be empty"
    );
    println!("  Transaction ID: {}", tx_id_sol);
    println!("  Output Amount: {} SOL lamports", route_sol.out_amount);
    println!(
        "  Actual Slippage: {} bps ({:.2}%)",
        route_sol.slippage_bps,
        route_sol.slippage_bps as f64 / 100.0
    );
    if route_sol.slippage_bps > slippage_bps {
        println!(
            "  ⚠️  Warning: Actual slippage ({}) exceeds max slippage ({})",
            route_sol.slippage_bps, slippage_bps
        );
    }
    println!();
    println!("Round-trip summary:");
    println!("  Started with: {} SOL lamports", amount_lamports);
    println!("  Ended with: {} SOL lamports", route_sol.out_amount);
    println!(
        "  Difference: {} SOL lamports",
        route_sol.out_amount as i64 - amount_lamports as i64
    );

    // Clean up
    client.close().await?;

    Ok(())
}
