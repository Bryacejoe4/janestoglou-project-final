// src/strategy/gembot.rs — FIXED
// Fixes vs uploaded version:
//   • sol_price_usd  → sol_usd  (matches dashboard.rs field name)
//   • TradeRecord now includes pnl_usd field (matches dashboard.rs struct)
//   • telegram alert_bot_started uses correct field name

use std::collections::HashMap;
use tokio::sync::mpsc;
use anyhow::Result;
use solana_sdk::{pubkey::Pubkey, signature::Signer};
use std::str::FromStr;
use std::sync::Arc;

use crate::{
    config::{BotConfig, StrategyConfig},
    dashboard::{DashboardState, TradeRecord},
    engine::TradingEngine,
    logic::{max_sol_cost_with_slippage, min_amount_out_after_slippage, Position, PositionEntry},
    strategy::filters::{FilterConfig, FilterVerdict, TokenFilter, TokenSnapshot},
    telegram::TelegramBot,
    utils,
    wallet::WalletManager,
};

#[derive(Debug)]
pub enum StrategyEvent {
    NewToken(TokenSnapshot),
    PriceTick { mint: String, price_sol: f64 },
    TokenGraduated { mint: String },
    CopySell { mint: String },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenDex { PumpFun, Raydium }

pub struct GembotStrategy {
    config:       StrategyConfig,
    engine:       TradingEngine,
    wallets:      WalletManager,
    filter:       TokenFilter,
    positions:    HashMap<String, Position>,
    entry_counts: HashMap<String, u8>,
    token_dex:    HashMap<String, TokenDex>,
    wallet_index: usize,
    dashboard:    Arc<DashboardState>,
    telegram:     TelegramBot,
}

impl GembotStrategy {
    pub fn new(
        bot_cfg:   &BotConfig,
        engine:    TradingEngine,
        wallets:   WalletManager,
        dashboard: Arc<DashboardState>,
    ) -> Self {
        let sc = &bot_cfg.sniper;
        let filter = TokenFilter::new(FilterConfig {
            min_volume_usd_5m:     sc.min_volume_usd,
            min_liquidity_sol:     sc.min_liquidity_lamports as f64 / 1e9,
            max_fresh_wallet_pct:  sc.max_fresh_wallet_pct,
            max_sniper_bundle_pct: sc.max_sniper_bundle_pct,
            max_top10_pct:         0.40,
            min_holder_count:      50,
            min_age_seconds:       30,
        });
        Self {
            config: bot_cfg.strategy.clone(),
            engine, wallets, filter,
            positions:    HashMap::new(),
            entry_counts: HashMap::new(),
            token_dex:    HashMap::new(),
            wallet_index: 0,
            dashboard,
            telegram: TelegramBot::from_env(),
        }
    }

    pub async fn run(mut self, mut rx: mpsc::Receiver<StrategyEvent>) {
        self.engine.init().await;

        let start_bal = self.engine.sol_balance(&self.wallets.main().pubkey()).await.unwrap_or(0);
        let start_sol = utils::lamports_to_sol(start_bal);
        self.dashboard.update_balance(start_sol);
        *self.dashboard.start_balance.write() = start_sol;

        // FIX: was sol_price_usd, correct field is sol_usd
        let sol_usd = *self.dashboard.sol_usd.read();
        self.telegram.alert_bot_started(
            &self.wallets.main().pubkey().to_string(),
            start_sol,
            sol_usd,
        ).await;

        tracing::info!("Gembot strategy running…");

        while let Some(event) = rx.recv().await {
            let result = match event {
                StrategyEvent::NewToken(snap)                => self.handle_new_token(snap).await,
                StrategyEvent::PriceTick { mint, price_sol } => self.handle_price_tick(&mint, price_sol).await,
                StrategyEvent::TokenGraduated { mint }       => self.handle_graduation(&mint).await,
                StrategyEvent::CopySell { mint }             => self.handle_copy_sell(&mint).await,
                StrategyEvent::Shutdown => { tracing::warn!("Shutdown."); break; }
            };
            if let Err(e) = result {
                tracing::error!("Strategy error: {}", e);
                self.dashboard.record_error();
            }
        }
    }

    fn next_wallet_idx(&mut self) -> usize {
        let idx = self.wallet_index % self.wallets.len();
        self.wallet_index += 1;
        idx
    }

    async fn handle_new_token(&mut self, snap: TokenSnapshot) -> Result<()> {
        if let FilterVerdict::Reject(reason) = self.filter.evaluate(&snap) {
            tracing::debug!("SKIP {}… — {}", &snap.mint[..8.min(snap.mint.len())], reason);
            return Ok(());
        }
        if *self.entry_counts.get(&snap.mint).unwrap_or(&0) >= self.config.max_entries_per_token {
            return Ok(());
        }

        tracing::info!("✅ ENTRY: {}…", &snap.mint[..8.min(snap.mint.len())]);

        let kp_idx  = self.next_wallet_idx();
        let keypair = self.wallets.get(kp_idx).unwrap_or_else(|| self.wallets.main());
        let payer   = keypair.pubkey();
        let mint_pk = Pubkey::from_str(&snap.mint)?;

        let sol_balance = self.engine.sol_balance(&payer).await?;
        let available   = sol_balance.saturating_sub(10_000_000);
        let trade_sol   = if self.config.entry_split { available / 2 } else { available };
        let trade_sol   = trade_sol.min(self.engine.config.risk.max_sol_per_trade);

        if trade_sol < 1_000_000 {
            tracing::warn!("Insufficient balance for {}…", &snap.mint[..8.min(snap.mint.len())]);
            return Ok(());
        }

        let tokens_estimate = if snap.price_sol > 0.0 {
            ((utils::lamports_to_sol(trade_sol) / snap.price_sol) * 1_000_000.0) as u64
        } else { 1_000_000 };

        let max_cost = max_sol_cost_with_slippage(trade_sol, self.config.slippage_bps);

        match self.engine.pump_buy(keypair, &mint_pk, tokens_estimate, max_cost).await {
            Ok(sig) => {
                tracing::info!("BUY ✓ {}", sig);
                // FIX: was sol_price_usd, correct field is sol_usd
                let sol_usd = *self.dashboard.sol_usd.read();
                self.telegram.alert_buy(&snap.mint, utils::lamports_to_sol(trade_sol), tokens_estimate, sol_usd, &sig).await;

                // FIX: TradeRecord now includes pnl_usd field
                self.dashboard.record_trade(TradeRecord {
                    mint:       snap.mint.clone(),
                    action:     "BUY".into(),
                    sol_amount: utils::lamports_to_sol(trade_sol),
                    pnl_sol:    None,
                    pnl_usd:    None,
                    sig,
                    timestamp:  chrono::Utc::now(),
                });
                let position = self.positions
                    .entry(snap.mint.clone())
                    .or_insert_with(|| Position::new(&snap.mint));
                position.entries.push(PositionEntry {
                    sol_spent: trade_sol, tokens_bought: tokens_estimate, entry_price: snap.price_sol,
                });
                position.peak_price_sol = snap.price_sol;
                position.is_closed      = false;
                self.dashboard.positions.insert(snap.mint.clone(), position.clone());
                *self.entry_counts.entry(snap.mint.clone()).or_insert(0) += 1;
                self.token_dex.insert(snap.mint.clone(), TokenDex::PumpFun);
                let new_bal = self.engine.sol_balance(&payer).await.unwrap_or(0);
                self.dashboard.update_balance(utils::lamports_to_sol(new_bal));
            }
            Err(e) => {
                tracing::error!("BUY failed: {}", e);
                self.dashboard.record_error();
            }
        }
        Ok(())
    }

    async fn handle_price_tick(&mut self, mint_str: &str, price_sol: f64) -> Result<()> {
        let Some(position) = self.positions.get_mut(mint_str) else {
            return self.check_second_entry(mint_str, price_sol).await;
        };
        if position.is_closed { return Ok(()); }

        position.update_peak(price_sol);
        let multiplier = position.pnl_multiplier(price_sol);

        if multiplier >= 1.0 + self.config.take_profit_pct {
            tracing::info!("🎯 TP {}… +{:.1}%", &mint_str[..8.min(mint_str.len())], (multiplier-1.0)*100.0);
            self.exit_position(mint_str, price_sol, false, "Take Profit").await?;
        } else if position.trailing_stop_triggered(price_sol, self.config.trailing_stop_pct) {
            tracing::warn!("📉 TRAIL STOP {}…", &mint_str[..8.min(mint_str.len())]);
            self.exit_position(mint_str, price_sol, true, "Trailing Stop").await?;
        } else if multiplier <= 1.0 - self.config.stop_loss_pct {
            tracing::warn!("🛑 STOP LOSS {}… -{:.1}%", &mint_str[..8.min(mint_str.len())], (1.0-multiplier)*100.0);
            self.exit_position(mint_str, price_sol, true, "Stop Loss").await?;
        }
        Ok(())
    }

    async fn handle_graduation(&mut self, mint_str: &str) -> Result<()> {
        tracing::info!("🎓 {}… graduated → Raydium", &mint_str[..8.min(mint_str.len())]);
        self.token_dex.insert(mint_str.to_string(), TokenDex::Raydium);
        self.telegram.alert_graduation(mint_str).await;
        if let Some(pos) = self.positions.get(mint_str) {
            self.dashboard.positions.insert(mint_str.to_string(), pos.clone());
        }
        Ok(())
    }

    async fn handle_copy_sell(&mut self, mint_str: &str) -> Result<()> {
        if !self.positions.contains_key(mint_str) { return Ok(()); }
        tracing::info!("📋 COPY SELL: exiting {}…", &mint_str[..8.min(mint_str.len())]);
        self.exit_position(mint_str, 0.0, true, "Copy Sell").await
    }

    async fn check_second_entry(&mut self, mint_str: &str, current_price: f64) -> Result<()> {
        if !self.config.entry_split { return Ok(()); }
        if *self.entry_counts.get(mint_str).unwrap_or(&0) != 1 { return Ok(()); }
        let first_price = self.positions.get(mint_str)
            .and_then(|p| p.entries.first().map(|e| e.entry_price)).unwrap_or(0.0);
        if first_price == 0.0 { return Ok(()); }
        let dip = (first_price - current_price) / first_price;
        if dip < self.config.second_entry_dip_pct { return Ok(()); }

        tracing::info!("📌 2ND ENTRY {}… dip={:.1}%", &mint_str[..8.min(mint_str.len())], dip*100.0);

        let keypair  = self.wallets.main();
        let bal      = self.engine.sol_balance(&keypair.pubkey()).await?;
        let trade    = (bal / 2).min(self.engine.config.risk.max_sol_per_trade);
        let tokens   = if current_price > 0.0 { ((utils::lamports_to_sol(trade) / current_price) * 1_000_000.0) as u64 } else { 1_000_000 };
        let max_cost = max_sol_cost_with_slippage(trade, self.config.slippage_bps);
        let mint_pk  = Pubkey::from_str(mint_str)?;

        if let Ok(sig) = self.engine.pump_buy(keypair, &mint_pk, tokens, max_cost).await {
            tracing::info!("2ND ENTRY ✓ {}", sig);
            let sol_usd = *self.dashboard.sol_usd.read();
            self.telegram.alert_buy(mint_str, utils::lamports_to_sol(trade), tokens, sol_usd, &sig).await;
            self.dashboard.record_trade(TradeRecord {
                mint: mint_str.to_string(), action: "BUY2".into(),
                sol_amount: utils::lamports_to_sol(trade),
                pnl_sol: None, pnl_usd: None, sig, timestamp: chrono::Utc::now(),
            });
            if let Some(p) = self.positions.get_mut(mint_str) {
                p.entries.push(PositionEntry { sol_spent: trade, tokens_bought: tokens, entry_price: current_price });
                self.dashboard.positions.insert(mint_str.to_string(), p.clone());
            }
            *self.entry_counts.entry(mint_str.to_string()).or_insert(0) += 1;
        }
        Ok(())
    }

    async fn exit_position(&mut self, mint_str: &str, price_sol: f64, full_exit: bool, reason: &str) -> Result<()> {
        let keypair = self.wallets.main();
        let mint_pk = Pubkey::from_str(mint_str)?;
        let balance = self.engine.token_balance(&keypair.pubkey(), &mint_pk).await?;

        if balance == 0 {
            if let Some(p) = self.positions.get_mut(mint_str) { p.is_closed = true; }
            return Ok(());
        }

        let sell_amount = if full_exit || self.config.moonbag_pct == 0.0 {
            balance
        } else {
            let keep = (balance as f64 * self.config.moonbag_pct) as u64;
            balance.saturating_sub(keep)
        };

        let min_sol = if price_sol > 0.0 {
            min_amount_out_after_slippage(
                (sell_amount as f64 * price_sol * 1e-6 * 1e9) as u64,
                self.config.slippage_bps,
            )
        } else { 0 };

        let sol_in  = self.positions.get(mint_str).map(|p| utils::lamports_to_sol(p.total_sol_spent())).unwrap_or(0.0);
        let sol_out = sell_amount as f64 * price_sol / 1_000_000.0;
        let pnl_sol = if price_sol > 0.0 { sol_out - sol_in } else { 0.0 };
        let sol_usd = *self.dashboard.sol_usd.read();
        let pnl_usd = if sol_usd > 0.0 { Some(pnl_sol * sol_usd) } else { None };

        let dex = self.token_dex.get(mint_str).cloned().unwrap_or(TokenDex::PumpFun);
        let sig_result = match dex {
            TokenDex::PumpFun => self.engine.pump_sell(keypair, &mint_pk, sell_amount, min_sol).await,
            TokenDex::Raydium => {
                tracing::info!("Routing SELL via Raydium for {}…", &mint_str[..8.min(mint_str.len())]);
                self.engine.raydium_swap(keypair, mint_str, sell_amount, min_sol, false).await
            }
        };

        match sig_result {
            Ok(sig) => {
                tracing::info!("SELL ✓ {} | PnL: {:+.4} SOL", sig, pnl_sol);
                self.telegram.alert_sell(mint_str, pnl_sol, sol_usd, reason, &sig).await;
                self.dashboard.record_trade(TradeRecord {
                    mint: mint_str.to_string(), action: "SELL".into(),
                    sol_amount: sol_out, pnl_sol: Some(pnl_sol), pnl_usd,
                    sig, timestamp: chrono::Utc::now(),
                });
                if let Some(p) = self.positions.get_mut(mint_str) {
                    p.is_closed = full_exit || sell_amount == balance;
                    self.dashboard.positions.insert(mint_str.to_string(), p.clone());
                }
                let new_bal = self.engine.sol_balance(&keypair.pubkey()).await.unwrap_or(0);
                self.dashboard.update_balance(utils::lamports_to_sol(new_bal));
            }
            Err(e) => {
                tracing::error!("SELL failed {}…: {}", &mint_str[..8.min(mint_str.len())], e);
                self.dashboard.record_error();
            }
        }
        Ok(())
    }
}
