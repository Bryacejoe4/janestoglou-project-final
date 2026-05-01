// src/main.rs — Phase 2 complete
pub mod config;
pub mod wallet;
pub mod utils;
pub mod logic;
pub mod engine;
pub mod dex;
pub mod strategy;
pub mod sniper;
pub mod monitor;
pub mod copy_trade;
pub mod market_data;
pub mod dashboard;
pub mod telegram;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use std::str::FromStr;
use solana_sdk::signature::Signer;
use std::sync::Arc;
use std::collections::HashSet;

use config::BotConfig;
use engine::TradingEngine;
use wallet::WalletManager;
use strategy::gembot::{GembotStrategy, StrategyEvent};
use dashboard::{DashboardState, run_dashboard};

#[derive(Parser)]
#[command(
    name    = "botx",
    version = "0.3.0",
    about   = "Solana HFT Bot — Phase 2 Complete",
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the full automated bot (sniper + strategy + copy-trade + dashboard)
    Run,
    /// Manually buy a token on Pump.fun
    Buy {
        mint: String,
        #[arg(long, default_value_t = 100_000_000)]
        lamports: u64,
    },
    /// Manually sell ALL tokens for a given mint
    Sell {
        mint: String,
        #[arg(long, default_value_t = 0)]
        min_sol: u64,
    },
    /// Show wallet balance(s)
    Balance,
    /// Run offline sanity tests (no real transactions)
    Test,
    /// Scan Pump.fun activity (read-only, deduplicated)
    Scan,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("botx=info,warn")),
        )
        .with_target(false)
        .with_level(true)
        .init();

    let cli = Cli::parse();
    let cfg = BotConfig::load()?;

    match cli.command {

        // ── Balance ───────────────────────────────────────────────────────
        Command::Balance => {
            let wallets = WalletManager::from_env()?;
            let engine  = TradingEngine::new(cfg);
            engine.init().await;
            for i in 0..wallets.len() {
                let kp  = wallets.get(i).unwrap();
                let bal = engine.sol_balance(&kp.pubkey()).await?;
                println!(
                    "Wallet {} | {} | {:.6} SOL",
                    i + 1, kp.pubkey(),
                    utils::lamports_to_sol(bal),
                );
            }
        }

        // ── Manual Buy ────────────────────────────────────────────────────
        Command::Buy { mint, lamports } => {
            let wallets  = WalletManager::from_env()?;
            let engine   = TradingEngine::new(cfg.clone());
            engine.init().await;

            let mint_pk = solana_sdk::pubkey::Pubkey::from_str(&mint)
                .map_err(|_| anyhow::anyhow!(
                    "Invalid mint address: {}\nNote: use a token mint address, not a transaction signature.",
                    mint
                ))?;

            // Check balance before attempting
            let bal = engine.sol_balance(&wallets.main().pubkey()).await?;
            let needed = lamports + 15_000_000 + 1_000_000; // buy + jito tip + fees
            if bal < needed {
                println!(
                    "❌ Insufficient balance.\n   Have:  {:.6} SOL\n   Need:  {:.6} SOL (buy + tip + fees)",
                    utils::lamports_to_sol(bal),
                    utils::lamports_to_sol(needed),
                );
                return Ok(());
            }

            let max_cost = logic::max_sol_cost_with_slippage(lamports, cfg.strategy.slippage_bps);
            let sig = engine.pump_buy(wallets.main(), &mint_pk, 1_000_000, max_cost).await?;
            println!("BUY submitted: https://solscan.io/tx/{}", sig);
        }

        // ── Manual Sell ───────────────────────────────────────────────────
        Command::Sell { mint, min_sol } => {
            let wallets = WalletManager::from_env()?;
            let engine  = TradingEngine::new(cfg);
            engine.init().await;

            let mint_pk = solana_sdk::pubkey::Pubkey::from_str(&mint)
                .map_err(|_| anyhow::anyhow!(
                    "Invalid mint address: {}\nNote: use a token mint address, not a transaction signature.",
                    mint
                ))?;

            let balance = engine.token_balance(&wallets.main().pubkey(), &mint_pk).await?;
            if balance == 0 {
                println!("No tokens to sell for {}", mint);
                return Ok(());
            }
            println!("Selling {} tokens…", balance);
            let sig = engine.pump_sell(wallets.main(), &mint_pk, balance, min_sol).await?;
            println!("SELL submitted: https://solscan.io/tx/{}", sig);
        }

        // ── Full Automated Run ────────────────────────────────────────────
        Command::Run => {
            let wallets   = WalletManager::from_env()?;
            let engine    = TradingEngine::new(cfg.clone());
            engine.init().await;

            let start_bal = engine.sol_balance(&wallets.main().pubkey()).await?;
            tracing::info!(
                "🚀 Bot v0.3.0 | Wallet: {} | Balance: {:.4} SOL",
                wallets.main().pubkey(),
                utils::lamports_to_sol(start_bal),
            );

            let dashboard    = DashboardState::new();
            let (tx, rx)     = mpsc::channel::<StrategyEvent>(1024);
            let strategy     = GembotStrategy::new(&cfg, engine, wallets, Arc::clone(&dashboard));
            let strat_handle = tokio::spawn(strategy.run(rx));

            if cfg.sniper.enabled {
                let sn = sniper::Sniper::new(cfg.clone(), tx.clone());
                tokio::spawn(async move {
                    if let Err(e) = sn.run().await { tracing::error!("Sniper: {}", e); }
                });
            } else {
                tracing::info!("Sniper disabled");
            }

            if cfg.copy_trade.enabled {
                let ct = copy_trade::CopyTrader::new(cfg.clone(), tx.clone());
                tokio::spawn(async move {
                    if let Err(e) = ct.run().await { tracing::error!("CopyTrader: {}", e); }
                });
            }

            tokio::spawn(run_dashboard(Arc::clone(&dashboard), 15));

            println!("✅ Bot v0.3.0 running. Press Ctrl+C to stop.");
            tokio::signal::ctrl_c().await?;
            tracing::info!("Shutting down…");
            let _ = tx.send(StrategyEvent::Shutdown).await;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5), strat_handle,
            ).await;
            println!("Stopped cleanly.");
        }

        // ── Scan (deduplicated) ───────────────────────────────────────────
        Command::Scan => {
            tracing::info!("📡 Scanning Pump.fun activity (read-only)…");
            let engine = TradingEngine::new(cfg);
            let pump   = solana_sdk::pubkey::Pubkey::from_str(dex::pumpfun::PUMP_PROGRAM_ID)?;
            let mut seen: HashSet<String> = HashSet::new();

            loop {
                match engine.rpc.get_signatures_for_address(&pump).await {
                    Ok(sigs) => {
                        let mut new_count = 0;
                        for sig_info in sigs.into_iter().take(20) {
                            let sig = sig_info.signature.clone();
                            if seen.insert(sig.clone()) {
                                println!("🔎 https://solscan.io/tx/{}", sig);
                                new_count += 1;
                            }
                        }
                        // Keep seen set from growing unbounded
                        if seen.len() > 200 { seen.clear(); }
                        if new_count == 0 {
                            println!("⏳ No new activity… polling");
                        }
                    }
                    Err(e) => tracing::warn!("get_signatures: {}", e),
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }

        // ── Offline Tests ─────────────────────────────────────────────────
        Command::Test => { run_offline_tests(); }
    }

    Ok(())
}

fn run_offline_tests() {
    use logic::{
        Logic, PoolReserves, PumpCurveState,
        min_amount_out_after_slippage, max_sol_cost_with_slippage,
    };
    use strategy::filters::{FilterConfig, FilterVerdict, TokenFilter, TokenSnapshot};
    use strategy::risk::RiskManager;
    use config::RiskConfig;
    use dex::pumpfun::TokenProgram;

    println!("\n══════════════════════════════════════════════");
    println!("  solana-hft-botx v0.3.0 — Offline Test Suite");
    println!("══════════════════════════════════════════════\n");

    let reserves = PoolReserves { reserve_in: 1_000_000_000, reserve_out: 150_000_000_000 };
    let quote    = Logic::calculate_amm_quote(100_000_000, &reserves, 100).unwrap();
    println!("✅ [1/7] AMM Quote: {} expected | {} min", quote.expected_amount_out, quote.min_amount_out);
    assert!(quote.expected_amount_out > 0);

    let curve = PumpCurveState::default();
    println!("✅ [2/7] Pump curve price: {:.9} SOL/token", curve.price_per_token());

    assert_eq!(min_amount_out_after_slippage(1_000_000, 100), 990_000);
    assert_eq!(max_sol_cost_with_slippage(1_000_000, 100),     1_010_000);
    println!("✅ [3/7] Slippage helpers correct");

    let good = TokenSnapshot {
        mint: "So11111111111111111111111111111111111111112".into(),
        volume_usd_5m: 35_000.0, liquidity_sol: 10.0, holder_count: 150,
        top10_pct: 0.28, fresh_wallet_pct: 0.15, sniper_bundle_pct: 0.08,
        age_seconds: 60, organic_chart: true, price_sol: 0.0001,
        ..Default::default()
    };
    let filter = TokenFilter::new(FilterConfig::default());
    assert_eq!(filter.evaluate(&good), FilterVerdict::Pass);
    println!("✅ [4/7] Token filter PASS");

    let mut bad = good.clone();
    bad.sniper_bundle_pct = 0.60;
    assert!(matches!(filter.evaluate(&bad), FilterVerdict::Reject(_)));
    println!("✅ [5/7] Token filter REJECT");

    let mut rm = RiskManager::new(RiskConfig {
        max_position_pct: 0.15, daily_loss_limit_pct: 0.10, max_sol_per_trade: 500_000_000,
    }, 2_000_000_000);
    let _ = rm.allowed_trade_size(2_000_000_000).unwrap();
    rm.record_trade(-250_000_000);
    assert!(rm.is_paused());
    println!("✅ [6/7] Risk manager pauses on loss");

    assert_ne!(TokenProgram::Legacy.pubkey(), TokenProgram::Token2022.pubkey());
    println!("✅ [7/7] Token-2022 IDs distinct\n");

    println!("══════════════════════════════════════════════");
    println!("  ✅ ALL 7 TESTS PASSED");
    println!("══════════════════════════════════════════════\n");
}
