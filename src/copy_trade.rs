// src/copy_trade.rs 
// Mirrors BOTH buy and sell signals from watched wallets.

use anyhow::Result;
use tokio::sync::mpsc;

use crate::{
    config::BotConfig,
    strategy::{
        filters::TokenSnapshot,
        gembot::StrategyEvent,
    },
};

pub struct CopyTrader {
    config: BotConfig,
    tx:     mpsc::Sender<StrategyEvent>,
}

impl CopyTrader {
    pub fn new(config: BotConfig, tx: mpsc::Sender<StrategyEvent>) -> Self {
        Self { config, tx }
    }

    pub async fn run(self) -> Result<()> {
        if !self.config.copy_trade.enabled {
            tracing::info!("Copy trading disabled (set copy_trade.enabled = true in config)");
            return Ok(());
        }
        let watched = self.config.copy_trade.watched_wallets.clone();
        if watched.is_empty() {
            tracing::warn!("Copy trading enabled but no watched_wallets configured");
            return Ok(());
        }

        tracing::info!("Copy trader monitoring {} wallet(s)", watched.len());

        let wss_url  = self.config.wss_url.clone();
        let tx       = self.tx.clone();
        let size_pct = self.config.copy_trade.trade_size_pct;

        for wallet_str in watched {
            let wss = wss_url.clone();
            let tx2 = tx.clone();
            let addr = wallet_str.clone();
            tokio::task::spawn_blocking(move || {
                watch_wallet_blocking(&wss, &addr, tx2, size_pct)
            });
        }

        futures_util::future::pending::<()>().await;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Blocking wallet watcher — mirrors both buys AND sells
// ─────────────────────────────────────────────────────────────────────────────

fn watch_wallet_blocking(
    wss_url:  &str,
    wallet:   &str,
    tx:       mpsc::Sender<StrategyEvent>,
    _size_pct: f64,
) {
    use solana_client::pubsub_client::PubsubClient;
    use solana_client::rpc_config::{
        RpcTransactionLogsConfig, RpcTransactionLogsFilter,
    };
    use solana_sdk::commitment_config::CommitmentConfig;

    let filter = RpcTransactionLogsFilter::Mentions(vec![wallet.to_string()]);
    let config = RpcTransactionLogsConfig {
        commitment: Some(CommitmentConfig::confirmed()),
    };

    let (_sub, receiver) = match PubsubClient::logs_subscribe(wss_url, filter, config) {
        Ok(s)  => s,
        Err(e) => {
            tracing::error!("Copy trade subscribe for {}: {}", wallet, e);
            return;
        }
    };

    tracing::info!("Copy trade: watching {}…", &wallet[..8.min(wallet.len())]);

    loop {
        let response = match receiver.recv() {
            Ok(r)  => r,
            Err(_) => {
                tracing::warn!("Copy trade: lost connection to {}…", &wallet[..8.min(wallet.len())]);
                break;
            }
        };

        let logs: Vec<String> = response.value.logs;

        let is_pump_buy  = logs.iter().any(|l: &String| l.contains("Instruction: Buy"));
        let is_pump_sell = logs.iter().any(|l: &String| l.contains("Instruction: Sell"));

        if !is_pump_buy && !is_pump_sell { continue; }

        let mint = match extract_mint_from_logs(&logs) {
            Some(m) => m,
            None    => continue,
        };

        tracing::info!(
            "📋 COPY {} from {}… on {}…",
            if is_pump_buy { "BUY" } else { "SELL" },
            &wallet[..8.min(wallet.len())],
            &mint[..8.min(mint.len())]
        );

        if is_pump_buy {
            // Mirror the buy — let strategy filters decide if it's good
            let snap = TokenSnapshot {
                mint: mint.clone(),
                volume_usd_5m: 99_999.0,  // bypass volume filter for copy trades
                liquidity_sol: 10.0,
                holder_count:  100,
                organic_chart: true,
                fresh_wallet_pct:  0.0,
                sniper_bundle_pct: 0.0,
                top10_pct:         0.0,
                age_seconds:       60,
                price_sol:         0.0,
                ..Default::default()
            };
            let _ = tx.try_send(StrategyEvent::NewToken(snap));
        } else if is_pump_sell {
            // Mirror the sell — emit CopySell so strategy exits the position
            let _ = tx.try_send(StrategyEvent::CopySell { mint: mint.clone() });
        }
    }
}

fn extract_mint_from_logs(logs: &[String]) -> Option<String> {
    for line in logs {
        if let Some(pos) = line.find("mint: ") {
            let after = &line[pos + 6..];
            let mint: String = after.chars()
                .take_while(|c| c.is_alphanumeric())
                .collect();
            if mint.len() >= 32 { return Some(mint); }
        }
    }
    None
}
