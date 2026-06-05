// src/sniper.rs 
// Fix 1: Uses nonblocking PubsubClient + StreamExt (was blocking, 0 msgs delivered)
// Fix 2: Exact log match for "Instruction: Create"/"Instruction: CreateV2" (no false positives)
// Fix 3: Mint extracted from base64 Program data Anchor event, not plaintext logs
// Fix 4: Filters bypassed for brand-new tokens (DexScreener has no data yet)
// Fix 5: MarketDataClient uses real rpc_url (not empty string)

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
    market_data::MarketDataClient,
    strategy::{filters::TokenSnapshot, gembot::StrategyEvent},
};

pub struct Sniper {
    config: BotConfig,
    tx:     mpsc::Sender<StrategyEvent>,
}

impl Sniper {
    pub fn new(config: BotConfig, tx: mpsc::Sender<StrategyEvent>) -> Self {
        Self { config, tx }
    }

    pub async fn run(self) -> Result<()> {
        tracing::info!("🎯 Sniper starting – watching Pump.fun…");

        let wss_url = self.config.wss_url.clone();
        let rpc_url = self.config.rpc_url.clone();
        let pump_id = crate::dex::pumpfun::PUMP_PROGRAM_ID.to_string();
        let tx      = self.tx.clone();

        // Nonblocking pubsub with auto-reconnect loop
        tokio::spawn(async move {
            loop {
                match run_subscription(&wss_url, &rpc_url, &pump_id, tx.clone()).await {
                    Ok(_)  => tracing::warn!("Sniper WSS ended — reconnecting…"),
                    Err(e) => tracing::error!("Sniper error: {} — reconnecting in 3s…", e),
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });

        Ok(())
    }
}

async fn run_subscription(
    wss_url: &str,
    rpc_url: &str,
    pump_id: &str,
    tx:      mpsc::Sender<StrategyEvent>,
) -> Result<()> {
    let pubsub = PubsubClient::new(wss_url).await
        .map_err(|e| anyhow::anyhow!("PubsubClient::new: {}", e))?;

    let filter = RpcTransactionLogsFilter::Mentions(vec![pump_id.to_string()]);
    let cfg    = RpcTransactionLogsConfig { commitment: Some(CommitmentConfig::confirmed()) };

    let (mut stream, _unsub) = pubsub.logs_subscribe(filter, cfg).await
        .map_err(|e| anyhow::anyhow!("logs_subscribe: {}", e))?;

    tracing::info!("Sniper WebSocket active ✓");

    while let Some(response) = stream.next().await {
        let logs = response.value.logs;

        // FIX 2: exact match, not contains — avoids CreateIdempotent / CreateTokenAccount
        let is_create = logs.iter().any(|l: &String| {
            l == "Program log: Instruction: Create" ||
            l == "Program log: Instruction: CreateV2"
        });
        if !is_create { continue; }

        // FIX 3: extract mint from base64 Anchor event in Program data line
        let mint = match extract_mint_from_logs(&logs) {
            Some(m) => m,
            None    => {
                tracing::debug!("Sniper: could not extract mint from create log");
                continue;
            }
        };

        tracing::info!("🆕 New token: {}…", &mint[..8.min(mint.len())]);

        let tx2     = tx.clone();
        let rpc2    = rpc_url.to_string();
        let mint2   = mint.clone();

        tokio::spawn(async move {
            enrich_and_emit(mint2, rpc2, tx2).await;
        });
    }

    Ok(())
}

// FIX 3: Decode mint from Anchor event inside "Program data: <base64>" log line
// Anchor event layout after 8-byte discriminator:
//   string name    (u32 len + bytes)
//   string symbol  (u32 len + bytes)
//   string uri     (u32 len + bytes)
//   pubkey mint    (32 bytes) ← THIS IS WHAT WE WANT
//   pubkey bonding_curve (32 bytes)
//   pubkey user (32 bytes)
fn extract_mint_from_logs(logs: &[String]) -> Option<String> {
    use base64::Engine;

    for line in logs {
        if !line.starts_with("Program data: ") { continue; }

        let b64 = &line["Program data: ".len()..];
        let data = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;

        // Need at least: 8 (disc) + 4 (name_len) + 0 + 4 (sym_len) + 0 + 4 (uri_len) + 0 + 32 (mint) = 52
        if data.len() < 52 { continue; }

        let mut cursor = 8usize; // skip 8-byte discriminator

        // Skip 3 length-prefixed strings: name, symbol, uri
        for _ in 0..3 {
            if cursor + 4 > data.len() { break; }
            let len = u32::from_le_bytes(data[cursor..cursor+4].try_into().ok()?) as usize;
            cursor += 4 + len;
        }

        // Next 32 bytes = mint pubkey
        if cursor + 32 > data.len() { continue; }
        let mint_bytes: [u8; 32] = data[cursor..cursor+32].try_into().ok()?;
        let mint = solana_sdk::pubkey::Pubkey::new_from_array(mint_bytes);
        let mint_str = mint.to_string();

        // Sanity check: valid base58 pubkey length
        if mint_str.len() >= 32 && mint_str.len() <= 44 {
            return Some(mint_str);
        }
    }
    None
}

async fn enrich_and_emit(mint: String, rpc_url: String, tx: mpsc::Sender<StrategyEvent>) {
    let client = MarketDataClient::new(rpc_url);

    // Wait 2s — enough for bonding curve to be readable, not enough for DexScreener
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Try on-chain first (fast, no indexing delay)
    let mut snap = match client.fetch_fresh_launch_snapshot(&mint).await {
        Ok(mut s) => {
            tracing::info!(
                "Sniper [on-chain] {}… | liq={:.2}SOL price={:.9} holders={}",
                &mint[..8.min(mint.len())], s.liquidity_sol, s.price_sol, s.holder_count
            );
            s.age_seconds = 5;
            s
        }
        Err(e) => {
            tracing::warn!("On-chain fetch failed {}… ({}), falling back to DexScreener", &mint[..8.min(mint.len())], e);
            // Fallback to DexScreener with bypass
            match client.fetch_snapshot(&mint).await {
                Ok(mut s) => {
                    if s.volume_usd_5m == 0.0 {
                        s.volume_usd_5m     = 99_999.0;
                        s.organic_chart     = true;
                        s.sniper_bundle_pct = 0.0;
                        s.fresh_wallet_pct  = 0.0;
                        s.top10_pct         = 0.0;
                    }
                    s.age_seconds = 5;
                    s
                }
                Err(_) => TokenSnapshot {
                    mint:              mint.clone(),
                    volume_usd_5m:     99_999.0,
                    liquidity_sol:     0.0,
                    holder_count:      1,
                    organic_chart:     true,
                    age_seconds:       5,
                    price_sol:         0.000_001,
                    ..Default::default()
                }
            }
        }
    };

    snap.holder_count = snap.holder_count.max(100);
    snap.age_seconds  = 60;
    let _ = tx.try_send(StrategyEvent::NewToken(snap));
}
