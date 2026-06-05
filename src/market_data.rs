// src/market_data.rs
// Fix: SOL/USD now fetches from Binance API (primary) with DexScreener as fallback.
// Binance is more reliable and doesn't require any API key.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use base64::Engine;

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
        //tracing::debug!("Binance failed, trying DexScreener for SOL/USD…");
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
        if self.rpc_url.is_empty() { return Ok(0); }
        let payload = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTokenAccounts",
            "params": { "mint": mint, "limit": 1000, "options": { "showZeroBalance": false } }
        });
        let body: serde_json::Value = self.http
            .post(&self.rpc_url).json(&payload).send().await
            .map_err(|e| anyhow!("Helius: {}", e))?
            .json().await
            .map_err(|e| anyhow!("Helius parse: {}", e))?;
        Ok(body["result"]["total"].as_u64().unwrap_or(0) as u32)
    }

    /// Fresh launch snapshot — reads bonding curve on-chain via RPC.
    /// Tries both seed variants, falls back to Pump.fun genesis constants if not yet on-chain.
    pub async fn fetch_fresh_launch_snapshot(&self, mint: &str) -> Result<TokenSnapshot> {
        use solana_sdk::pubkey::Pubkey;
        use std::str::FromStr;

        if self.rpc_url.is_empty() {
            return Err(anyhow!("no rpc_url"));
        }

        let mint_pk      = Pubkey::from_str(mint).map_err(|_| anyhow!("invalid mint"))?;
        let pump_program = Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap();

        // Try both seeds — older tokens use "bonding-curve", newer CreateV2 tokens use "bonding-curve-v2"
        let mut parsed: Option<(f64, f64)> = None;
        for seed in [b"bonding-curve" as &[u8], b"bonding-curve-v2"] {
            let (bc_pda, _) = Pubkey::find_program_address(&[seed, mint_pk.as_ref()], &pump_program);
            let payload = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getAccountInfo",
                "params": [bc_pda.to_string(), {"encoding": "base64"}]
            });
            if let Ok(resp) = self.http.post(&self.rpc_url).json(&payload).send().await {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(b64) = body["result"]["value"]["data"][0].as_str() {
                        if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64) {
                            if raw.len() >= 49 {
                                let d = &raw[8..];
                                let ru = |o: usize| -> u64 {
                                    u64::from_le_bytes(d[o..o+8].try_into().unwrap_or([0u8;8]))
                                };
                                let vsr = ru(0);
                                let vtr = ru(8);
                                let rsr = ru(16);
                                let liq   = vsr as f64 / 1e9;
                                let price = if vtr > 0 {
                                    (vsr as f64 / 1e9) / (vtr as f64 / 1e6)
                                } else { 0.000_001 };
                                parsed = Some((liq, price));
                                break;
                            }
                        }
                    }
                }
            }
        }

        // If bonding curve not yet on-chain, use Pump.fun genesis constants (identical for every new token)
        let (liquidity_sol, price_sol) = parsed.unwrap_or_else(|| {
            let vsr: u64 = 30_000_000_000;
            let vtr: u64 = 1_073_000_000_000_000;
            let price = (vsr as f64 / 1e9) / (vtr as f64 / 1e6);
            tracing::debug!("Sniper: BC not yet on-chain for {}…, using genesis defaults", &mint[..8.min(mint.len())]);
            (vsr as f64 / 1e9, price)
        });

        //let holder_count = self.fetch_holder_count(mint).await.unwrap_or(1);

        let (quality, holder_count) = tokio::join!(
            self.fetch_launch_quality(mint),
            self.fetch_holder_count(mint),
        );
        let (sniper_bundle_pct, fresh_wallet_pct, top10_pct) = quality;
        let holder_count = holder_count.unwrap_or(1);

        tracing::info!(
            "Quality {}… sniper={:.0}% fresh={:.0}% top10={:.0}% holders={}",
            &mint[..8.min(mint.len())],
            sniper_bundle_pct * 100.0,
            fresh_wallet_pct  * 100.0,
            top10_pct         * 100.0,
            holder_count,
        );

        Ok(TokenSnapshot {
            mint:              mint.to_string(),
            volume_usd_5m:     99_999.0,
            liquidity_sol,
            price_sol,
            holder_count,
            age_seconds:       5,
            organic_chart:     true,
            fresh_wallet_pct,
            sniper_bundle_pct,
            top10_pct,
            ..Default::default()
        })
    }

    /// Score a fresh launch using on-chain data only.
    /// Populates: sniper_bundle_pct, fresh_wallet_pct, top10_pct
    /// All via Helius RPC — no third-party APIs needed.
    pub async fn fetch_launch_quality(&self, mint: &str) -> (f64, f64, f64) {
        // Returns (sniper_bundle_pct, fresh_wallet_pct, top10_pct)
        // Defaults to (0, 0, 0) — passes all filters — if any step fails.
        // This means a data failure never blocks a trade, it just skips quality gating.
        let (top10, _) = self.fetch_top10_concentration(mint).await;
        let (sniper_pct, fresh_pct) = self.fetch_bundle_and_fresh(mint).await;
        (sniper_pct, fresh_pct, top10)
    }

    async fn fetch_top10_concentration(&self, mint: &str) -> (f64, u64) {
        if self.rpc_url.is_empty() { return (0.0, 0); }
        let payload = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTokenAccounts",
            "params": {
                "mint": mint,
                "limit": 1000,
                "options": { "showZeroBalance": false }
            }
        });

        let Ok(resp) = self.http.post(&self.rpc_url).json(&payload).send().await else { return (0.0, 0); };
        let Ok(body) = resp.json::<serde_json::Value>().await else { return (0.0, 0); };
        let accounts = match body["result"]["token_accounts"].as_array() {
            Some(a) => a.clone(),
            None    => return (0.0, 0),
        };

        let mut balances: Vec<u64> = accounts.iter()
            .filter_map(|a| a["amount"].as_str()?.parse::<u64>().ok())
            .collect();

        if balances.is_empty() { return (0.0, 0); }
        let total: u64 = balances.iter().sum();

        if total == 0 { return (0.0, 0); }
        balances.sort_unstable_by(|a, b| b.cmp(a));
        let top10_sum: u64 = balances.iter().take(10).sum();
        (top10_sum as f64 / total as f64, total)
    }

    async fn fetch_bundle_and_fresh(&self, mint: &str) -> (f64, f64) {
        // (sniper_bundle_pct, fresh_wallet_pct)
        if self.rpc_url.is_empty() { return (0.0, 0.0); }

        // Step 1: Get first 20 transactions for this mint
        let payload = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getSignaturesForAddress",
            "params": [mint, {"limit": 20}]
        });

        let Ok(resp) = self.http.post(&self.rpc_url).json(&payload).send().await else { return (0.0, 0.0); };
        let Ok(body) = resp.json::<serde_json::Value>().await else { return (0.0, 0.0); };
        let sigs = match body["result"].as_array() {
            Some(s) => s.clone(),
            None    => return (0.0, 0.0),
        };

        if sigs.is_empty() { return (0.0, 0.0); }
        let mut buyer_wallets: Vec<String> = Vec::new();
        let mut slots: Vec<u64> = Vec::new();

        for sig_entry in sigs.iter().take(20) {
            let sig = match sig_entry["signature"].as_str() { Some(s) => s, None => continue };
            let slot = sig_entry["slot"].as_u64().unwrap_or(0);
            let tx_payload = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getTransaction",
                "params": [sig, {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0}]
            });
            let Ok(tx_resp) = self.http.post(&self.rpc_url).json(&tx_payload).send().await else { continue };
            let Ok(tx_body) = tx_resp.json::<serde_json::Value>().await else { continue };

            // Extract payer (fee payer = buyer wallet)
            if let Some(keys) = tx_body["result"]["transaction"]["message"]["accountKeys"].as_array() {
                if let Some(first) = keys.first() {
                    if let Some(pk) = first["pubkey"].as_str() {
                        buyer_wallets.push(pk.to_string());
                        slots.push(slot);
                    }
                }
            }
        }

        if buyer_wallets.is_empty() { return (0.0, 0.0); }
        // Bundle detection: if multiple buys happen within 2 slots, likely coordinated
        let mut bundle_count = 0usize;

        for i in 1..slots.len() {
            if slots[i-1].saturating_sub(slots[i]) <= 2 { bundle_count += 1; }
        }

        let sniper_pct = bundle_count as f64 / buyer_wallets.len() as f64;
        // Fresh wallet detection: check wallet age via getAccountInfo
        // A wallet is "fresh" if it has very few transactions (proxy for age)
        let mut fresh_count = 0usize;

        for wallet in buyer_wallets.iter().take(10) {
            let age_payload = serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getSignaturesForAddress",
                "params": [wallet, {"limit": 5}]
            });

            if let Ok(r) = self.http.post(&self.rpc_url).json(&age_payload).send().await {
                if let Ok(b) = r.json::<serde_json::Value>().await {
                    if let Some(list) = b["result"].as_array() {
                        if list.len() < 5 { fresh_count += 1; }
                    }
                }
            }
        }
        let fresh_pct = fresh_count as f64 / buyer_wallets.len().min(10) as f64;
        (sniper_pct, fresh_pct)
    }
}
