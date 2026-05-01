// src/monitor.rs
//   • Graduation detection — emits TokenGraduated when bonding curve is complete
//   • Uses DexScreener for price (faster) with bonding curve fallback
//   • SOL/USD price cached and refreshed every 60 seconds

use anyhow::Result;
use dashmap::DashMap;
use parking_lot::RwLock;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::sync::mpsc;

use crate::{
    config::BotConfig,
    dex::pumpfun,
    logic::PumpCurveState,
    market_data::MarketDataClient,
    strategy::gembot::StrategyEvent,
};

#[derive(Debug, Clone)]
pub struct PriceEntry {
    pub price_sol:    f64,
    pub last_updated: std::time::Instant,
    pub graduated:    bool,
}

pub struct Monitor {
    rpc:          RpcClient,
    market_data:  MarketDataClient,
    #[allow(dead_code)]
    config:       BotConfig,
    tx:           mpsc::Sender<StrategyEvent>,
    prices:       Arc<DashMap<String, PriceEntry>>,
    sol_usd:      Arc<RwLock<f64>>,
}

impl Monitor {
    pub fn new(config: BotConfig, tx: mpsc::Sender<StrategyEvent>) -> Self {
        let rpc = RpcClient::new_with_commitment(
            config.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        );
        let market_data = MarketDataClient::new(config.rpc_url.clone());
        Self {
            rpc,
            market_data,
            config,
            tx,
            prices:  Arc::new(DashMap::new()),
            sol_usd: Arc::new(RwLock::new(0.0)),
        }
    }

    /// One-shot SOL/USD fetch used by main.rs before the run loop starts.
    pub async fn fetch_sol_usd_once(&self) -> f64 {
        self.market_data.fetch_sol_price_usd().await.unwrap_or(0.0)
    }

    pub fn watch(&self, mint: &str) {
        tracing::info!("Monitor: watching {}…", &mint[..8.min(mint.len())]);
        self.prices.insert(mint.to_string(), PriceEntry {
            price_sol:    0.0,
            last_updated: std::time::Instant::now(),
            graduated:    false,
        });
    }

    pub fn unwatch(&self, mint: &str) {
        self.prices.remove(mint);
    }

    /// Returns the cached SOL/USD price.
    pub fn sol_usd(&self) -> f64 {
        *self.sol_usd.read()
    }

    pub async fn run(self) -> Result<()> {
        tracing::info!("Market monitor running…");

        let price_interval = Duration::from_millis(500);
        let sol_usd_interval = Duration::from_secs(60);
        let mut last_sol_usd = std::time::Instant::now();

        loop {
            // ── Refresh SOL/USD every 60 seconds ─────────────────────────
            if last_sol_usd.elapsed() >= sol_usd_interval {
                let price = self.market_data.fetch_sol_price_usd().await.unwrap_or(0.0);
                if price > 0.0 {
                    *self.sol_usd.write() = price;
                    tracing::debug!("SOL/USD: ${:.2}", price);
                }
                last_sol_usd = std::time::Instant::now();
            }

            // ── Price tick for all watched tokens ─────────────────────────
            let mints: Vec<String> = self.prices.iter().map(|e| e.key().clone()).collect();

            for mint_str in mints {
                // Skip tokens already known to be graduated — handled by engine
                let already_graduated = self.prices
                    .get(&mint_str)
                    .map(|e| e.graduated)
                    .unwrap_or(false);

                // ── Graduation check (on-chain bonding curve) ─────────────
                if !already_graduated {
                    if let Ok(true) = self.check_graduated(&mint_str).await {
                        tracing::info!(
                            "🎓 TOKEN GRADUATED: {}… moving to Raydium",
                            &mint_str[..8.min(mint_str.len())]
                        );
                        if let Some(mut entry) = self.prices.get_mut(&mint_str) {
                            entry.graduated = true;
                        }
                        let _ = self.tx.try_send(StrategyEvent::TokenGraduated {
                            mint: mint_str.clone(),
                        });
                        continue; // skip price tick this cycle
                    }
                }

                // ── Price fetch ───────────────────────────────────────────
                let price = match self.market_data.fetch_price_sol(&mint_str).await {
                    Ok(p) if p > 0.0 => p,
                    _ => match self.fetch_price_from_chain(&mint_str).await {
                        Ok(p)  => p,
                        Err(e) => {
                            tracing::warn!(
                                "Price fetch failed for {}…: {}",
                                &mint_str[..8.min(mint_str.len())], e
                            );
                            continue;
                        }
                    }
                };

                self.check_risk_signals(&mint_str, price);

                if let Some(mut entry) = self.prices.get_mut(&mint_str) {
                    entry.price_sol    = price;
                    entry.last_updated = std::time::Instant::now();
                }

                let _ = self.tx.try_send(StrategyEvent::PriceTick {
                    mint:      mint_str.clone(),
                    price_sol: price,
                });
            }

            tokio::time::sleep(price_interval).await;
        }
    }

    // ── Check if bonding curve is complete (graduated) ────────────────────

    async fn check_graduated(&self, mint_str: &str) -> Result<bool> {
        let mint  = Pubkey::from_str(mint_str)?;
        let curve = pumpfun::bonding_curve_pda(&mint);
        let data  = self.rpc.get_account_data(&curve).await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let state = parse_bonding_curve(&data)?;
        Ok(state.complete)
    }

    // ── Bonding curve price fallback ──────────────────────────────────────

    async fn fetch_price_from_chain(&self, mint_str: &str) -> Result<f64> {
        let mint  = Pubkey::from_str(mint_str)?;
        let curve = pumpfun::bonding_curve_pda(&mint);
        let data  = self.rpc.get_account_data(&curve).await
            .map_err(|e| anyhow::anyhow!("get_account_data: {}", e))?;
        let state = parse_bonding_curve(&data)?;
        Ok(state.price_per_token())
    }

    // ── Risk signal detection ─────────────────────────────────────────────

    fn check_risk_signals(&self, mint_str: &str, current: f64) {
        if let Some(entry) = self.prices.get(mint_str) {
            let prev = entry.price_sol;
            if prev == 0.0 { return; }
            let change = (current - prev) / prev;
            if change < -0.20 {
                tracing::warn!(
                    "⚠️  RISK: {}… dropped {:.1}% in last tick",
                    &mint_str[..8.min(mint_str.len())], change * 100.0
                );
            }
            if change > 0.30 {
                tracing::warn!(
                    "⚠️  RISK: {}… pumped {:.1}% — possible distribution",
                    &mint_str[..8.min(mint_str.len())], change * 100.0
                );
            }
        }
    }
}

fn parse_bonding_curve(data: &[u8]) -> Result<PumpCurveState> {
    if data.len() < 49 {
        return Err(anyhow::anyhow!("bonding curve too short: {} bytes", data.len()));
    }
    let d = &data[8..];
    let read_u64 = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    Ok(PumpCurveState {
        virtual_sol_reserves:   read_u64(0),
        virtual_token_reserves: read_u64(8),
        real_sol_reserves:      read_u64(16),
        real_token_reserves:    read_u64(24),
        token_total_supply:     read_u64(32),
        complete:               d[40] != 0,
    })
}
