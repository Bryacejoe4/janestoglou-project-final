// src/copy_trade.rs 
// Same WSS fix as sniper.rs — blocking pubsub had silent message delivery failure

use anyhow::Result;
use futures_util::StreamExt;
use solana_client::{
    nonblocking::pubsub_client::PubsubClient,
    rpc_config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter},
};
use solana_sdk::commitment_config::CommitmentConfig;
use tokio::sync::mpsc;

use crate::{
    config::BotConfig,
    strategy::{filters::TokenSnapshot, gembot::StrategyEvent},
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
            tracing::info!("Copy trading disabled");
            return Ok(());
        }
        let watched = self.config.copy_trade.watched_wallets.clone();
        if watched.is_empty() {
            tracing::warn!("Copy trading enabled but no watched_wallets configured");
            return Ok(());
        }

        tracing::info!("Copy trader monitoring {} wallet(s)", watched.len());

        for wallet in watched {
            let wss_url = self.config.wss_url.clone();
            let tx2     = self.tx.clone();
            let w       = wallet.clone();
            tokio::spawn(async move {
                loop {
                    match watch_wallet_async(&wss_url, &w, tx2.clone()).await {
                        Ok(_)  => tracing::warn!("Copy trade {}… subscription ended — reconnecting…", &w[..8.min(w.len())]),
                        Err(e) => tracing::error!("Copy trade {}… error: {} — reconnecting in 3s…", &w[..8.min(w.len())], e),
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            });
        }

        // Keep alive
        futures_util::future::pending::<()>().await;
        Ok(())
    }
}

async fn watch_wallet_async(
    wss_url: &str,
    wallet:  &str,
    tx:      mpsc::Sender<StrategyEvent>,
) -> Result<()> {
    let pubsub = PubsubClient::new(wss_url).await
        .map_err(|e| anyhow::anyhow!("PubsubClient::new: {}", e))?;

    let filter = RpcTransactionLogsFilter::Mentions(vec![wallet.to_string()]);
    let config = RpcTransactionLogsConfig {
        commitment: Some(CommitmentConfig::confirmed()),
    };

    let (mut stream, _unsub) = pubsub.logs_subscribe(filter, config).await
        .map_err(|e| anyhow::anyhow!("logs_subscribe for {}…: {}", &wallet[..8.min(wallet.len())], e))?;

    tracing::info!("Copy trade watching {}… ✓", &wallet[..8.min(wallet.len())]);

    while let Some(response) = stream.next().await {
        let logs: Vec<String> = response.value.logs;

        let is_buy  = logs.iter().any(|l: &String| l.contains("Instruction: Buy") || l.contains("Instruction: BuyExactSolIn"));
        let is_sell = logs.iter().any(|l: &String| l.contains("Instruction: Sell"));

        if !is_buy && !is_sell { continue; }

        let mint = match extract_mint(&logs) {
            Some(m) => m,
            None    => continue,
        };

        tracing::info!("📋 COPY {} from {}… on {}…",
            if is_buy { "BUY" } else { "SELL" },
            &wallet[..8.min(wallet.len())],
            &mint[..8.min(mint.len())]);

        if is_buy {
            let snap = TokenSnapshot {
                mint:              mint.clone(),
                volume_usd_5m:     99_999.0,
                liquidity_sol:     10.0,
                holder_count:      100,
                organic_chart:     true,
                fresh_wallet_pct:  0.0,
                sniper_bundle_pct: 0.0,
                top10_pct:         0.0,
                age_seconds:       60,
                price_sol:         0.0,
                ..Default::default()
            };
            let _ = tx.try_send(StrategyEvent::NewToken(snap));
        }

        if is_sell {
            let _ = tx.try_send(StrategyEvent::CopySell { mint });
        }
    }

    Ok(())
}

fn extract_mint(logs: &[String]) -> Option<String> {
    for line in logs {
        if let Some(pos) = line.find("mint: ") {
            let after = &line[pos + 6..];
            let mint: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
            if mint.len() >= 32 { return Some(mint); }
        }
    }
    None
}
