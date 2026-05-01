// src/market_data.rs
// Fix: SOL/USD now fetches from Binance API (primary) with DexScreener as fallback.
// Binance is more reliable and doesn't require any API key.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

use crate::strategy::filters::TokenSnapshot;

const DEXSCREENER_URL: &str = "https://api.dexscreener.com/latest/dex/tokens";
const WSOL_MINT:       &str = "So11111111111111111111111111111111111111112";
// Binance ticker endpoint — no API key needed, always returns current price
const BINANCE_SOL_URL: &str = "https://api.binance.com/api/v3/ticker/price?symbol=SOLUSDT";

// ─────────────────────────────────────────────────────────────────────────────
//  Response types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DexResponse { pairs: Option<Vec<DexPair>> }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DexPair {
    chain_id:     Option<String>,
    price_usd:    Option<String>,
    price_native: Option<String>,
    volume:       Option<Volume>,
    price_change: Option<PriceChange>,
    txns:         Option<Txns>,
    liquidity:    Option<Liquidity>,
}

#[derive(Debug, Deserialize)]
struct BinanceTicker { price: String }

#[derive(Debug, Deserialize)] struct Volume      { m5: Option<f64>, h1: Option<f64> }
#[derive(Debug, Deserialize)] struct PriceChange { h1: Option<f64> }
#[derive(Debug, Deserialize)] struct Txns        { m5: Option<TxCount> }
#[derive(Debug, Deserialize)] struct TxCount     { buys: Option<u32>, sells: Option<u32> }
#[derive(Debug, Deserialize)] struct Liquidity   { quote: Option<f64> }

// ─────────────────────────────────────────────────────────────────────────────
//  MarketDataClient
// ─────────────────────────────────────────────────────────────────────────────

pub struct MarketDataClient {
    http:    Client,
    rpc_url: String,
}

impl MarketDataClient {
    pub fn new(rpc_url: String) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .user_agent("solana-hft-botx/0.3")
            .build()
            .expect("http client build");
        Self { http, rpc_url }
    }

    // ── Full snapshot ─────────────────────────────────────────────────────

    pub async fn fetch_snapshot(&self, mint: &str) -> Result<TokenSnapshot> {
        let (dex, holders) = tokio::join!(
            self.fetch_dexscreener(mint),
            self.fetch_holder_count(mint),
        );
        let mut snap = dex.unwrap_or_else(|_| {
            TokenSnapshot { mint: mint.to_string(), ..Default::default() }
        });
        snap.holder_count = holders.unwrap_or(0);
        Ok(snap)
    }

    // ── Price in SOL ──────────────────────────────────────────────────────

    pub async fn fetch_price_sol(&self, mint: &str) -> Result<f64> {
        let pair = self.best_solana_pair(mint).await?;
        pair.price_native
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&p| p > 0.0)
            .ok_or_else(|| anyhow!("no price_native for {}", mint))
    }

    // ── SOL/USD price — Binance primary, DexScreener fallback ─────────────

    pub async fn fetch_sol_price_usd(&self) -> Result<f64> {
        // Primary: Binance (fast, reliable, no key needed)
        if let Ok(price) = self.fetch_sol_usd_binance().await {
            tracing::debug!("SOL/USD from Binance: ${:.2}", price);
            return Ok(price);
        }

        // Fallback: DexScreener WSOL pairs
        tracing::debug!("Binance failed, trying DexScreener for SOL/USD…");
        self.fetch_sol_usd_dexscreener().await
    }

    async fn fetch_sol_usd_binance(&self) -> Result<f64> {
        let ticker: BinanceTicker = self.http
            .get(BINANCE_SOL_URL)
            .send().await
            .map_err(|e| anyhow!("Binance request: {}", e))?
            .json().await
            .map_err(|e| anyhow!("Binance parse: {}", e))?;

        let price = ticker.price.parse::<f64>()
            .map_err(|_| anyhow!("Binance price parse failed"))?;

        if price > 1.0 {
            Ok(price)
        } else {
            Err(anyhow!("Binance returned implausible SOL price: {}", price))
        }
    }

    async fn fetch_sol_usd_dexscreener(&self) -> Result<f64> {
        let body: DexResponse = self.http
            .get(&format!("{}/{}", DEXSCREENER_URL, WSOL_MINT))
            .send().await
            .map_err(|e| anyhow!("DexScreener SOL request: {}", e))?
            .json().await
            .map_err(|e| anyhow!("DexScreener SOL parse: {}", e))?;

        body.pairs.unwrap_or_default().iter()
            .filter(|p| p.chain_id.as_deref() == Some("solana"))
            .find_map(|p| {
                p.price_usd.as_deref()
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|&v| v > 1.0)
            })
            .ok_or_else(|| anyhow!("DexScreener: no valid SOL/USD price found"))
    }

    // ── Sniper % heuristic ────────────────────────────────────────────────

    pub async fn estimate_sniper_pct(&self, mint: &str) -> f64 {
        let Ok(pair) = self.best_solana_pair(mint).await else { return 0.0 };
        let buys  = pair.txns.as_ref().and_then(|t| t.m5.as_ref()).and_then(|t| t.buys).unwrap_or(0);
        let sells = pair.txns.as_ref().and_then(|t| t.m5.as_ref()).and_then(|t| t.sells).unwrap_or(0);
        let total = buys + sells;
        if total == 0 { return 0.0; }
        let r = buys as f64 / total as f64;
        if r > 0.80 && sells < 3 { r * 0.5 } else { 0.0 }
    }

    // ── Internal ──────────────────────────────────────────────────────────

    async fn fetch_dexscreener(&self, mint: &str) -> Result<TokenSnapshot> {
        let pair = self.best_solana_pair(mint).await?;

        let price_sol       = pair.price_native.as_deref()
            .and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let volume_5m       = pair.volume.as_ref().and_then(|v| v.m5).unwrap_or(0.0);
        let volume_1h       = pair.volume.as_ref().and_then(|v| v.h1).unwrap_or(0.0);
        let price_change_1h = pair.price_change.as_ref()
            .and_then(|p| p.h1).map(|p| p / 100.0).unwrap_or(0.0);
        let liquidity_sol   = pair.liquidity.as_ref()
            .and_then(|l| l.quote).unwrap_or(0.0);

        let buys  = pair.txns.as_ref().and_then(|t| t.m5.as_ref()).and_then(|t| t.buys).unwrap_or(0);
        let sells = pair.txns.as_ref().and_then(|t| t.m5.as_ref()).and_then(|t| t.sells).unwrap_or(0);
        let total = buys + sells;
        let organic_chart = if total > 0 {
            (buys as f64 / total as f64) < 0.95
        } else {
            true
        };

        Ok(TokenSnapshot {
            mint:             mint.to_string(),
            volume_usd_5m:    volume_5m,
            volume_usd_1h:    volume_1h,
            liquidity_sol,
            price_sol,
            price_change_1h,
            organic_chart,
            ..Default::default()
        })
    }

    async fn best_solana_pair(&self, mint: &str) -> Result<DexPair> {
        let body: DexResponse = self.http
            .get(&format!("{}/{}", DEXSCREENER_URL, mint))
            .send().await
            .map_err(|e| anyhow!("DexScreener request: {}", e))?
            .json().await
            .map_err(|e| anyhow!("DexScreener parse: {}", e))?;

        body.pairs.unwrap_or_default()
            .into_iter()
            .find(|p| p.chain_id.as_deref() == Some("solana"))
            .ok_or_else(|| anyhow!("no Solana pair on DexScreener for {}", mint))
    }

    async fn fetch_holder_count(&self, mint: &str) -> Result<u32> {
        // Skip holder count if rpc_url is empty (dashboard SOL price fetch)
        if self.rpc_url.is_empty() { return Ok(0); }

        let payload = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTokenAccounts",
            "params": {
                "mint": mint,
                "limit": 1000,
                "options": { "showZeroBalance": false }
            }
        });

        let body: serde_json::Value = self.http
            .post(&self.rpc_url)
            .json(&payload)
            .send().await
            .map_err(|e| anyhow!("Helius: {}", e))?
            .json().await
            .map_err(|e| anyhow!("Helius parse: {}", e))?;

        Ok(body["result"]["total"].as_u64().unwrap_or(0) as u32)
    }
}
