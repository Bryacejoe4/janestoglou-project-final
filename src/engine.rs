// src/engine.rs 

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::{v0::Message, VersionedMessage},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::VersionedTransaction,
};
use reqwest::Client;
use serde_json::json;
use std::str::FromStr;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::{
    config::BotConfig,
    dex::{pumpfun::{self, TokenProgram}, raydium},
    utils,
};

const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
];

const JITO_REGIONS: &[&str] = &[
    "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles",
    "https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles",
];

pub struct TradingEngine {
    pub rpc:       RpcClient,
    pub http:      Client,
    pub config:    BotConfig,
    fee_recipient: Arc<RwLock<Pubkey>>,
}

impl TradingEngine {
    pub fn new(config: BotConfig) -> Self {
        let rpc = RpcClient::new_with_commitment(
            config.rpc_url.clone(), CommitmentConfig::confirmed(),
        );
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build().expect("reqwest client");
        let fallback = Pubkey::from_str(pumpfun::FEE_RECIPIENT_FALLBACK).unwrap();
        Self { rpc, http, config, fee_recipient: Arc::new(RwLock::new(fallback)) }
    }

    pub async fn init(&self) {
        match pumpfun::fetch_fee_recipient(&self.rpc).await {
            Ok(pk) => { *self.fee_recipient.write() = pk; tracing::info!("✓ Fee recipient: {}", pk); }
            Err(e) => tracing::warn!("Fee recipient fallback ({})", e),
        }
    }

    fn fee_recip(&self) -> Pubkey { *self.fee_recipient.read() }

    // Probe which ATA the bonding curve actually uses on-chain
    async fn find_bonding_curve_ata(&self, mint: &Pubkey) -> Result<(Pubkey, TokenProgram)> {
        let bc = pumpfun::bonding_curve_pda(mint);
        for tok in [TokenProgram::Token2022, TokenProgram::Legacy] {
            let ata = utils::get_ata_with_program(&bc, mint, &tok.pubkey());
            if self.rpc.get_account(&ata).await.is_ok() {
                tracing::info!("Bonding curve ATA [{}] for {}…", tok.label(), &mint.to_string()[..8]);
                return Ok((ata, tok));
            }
        }
        Err(anyhow!("Bonding curve ATA not found for {}", &mint.to_string()[..8]))
    }

    // ── Pump.fun buy ──────────────────────────────────────────────────────
    pub async fn pump_buy(
        &self,
        keypair:      &Keypair,
        mint:         &Pubkey,
        token_amount: u64,
        max_sol_cost: u64,
    ) -> Result<String> {
        let payer = keypair.pubkey();
        let fee   = self.fee_recip();

        let (bc_ata, tok) = self.find_bonding_curve_ata(mint).await?;
        let bc_info = pumpfun::fetch_bonding_curve_info(&self.rpc, mint).await?;

        tracing::info!("BUY {}… tokens={} max={:.4}SOL [{}] cashback={}",
            utils::short_key(mint), token_amount,
            utils::lamports_to_sol(max_sol_cost), tok.label(),
            bc_info.cashback_enabled);

        let user_ata = utils::get_ata_with_program(&payer, mint, &tok.pubkey());

        let mut ixs = self.base_compute_ixs();
        ixs.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &payer, &payer, mint, &utils::to_spl_pubkey(&tok.pubkey()),
            )
        );
        ixs.push(pumpfun::build_buy_instruction(
            &payer, mint, token_amount, max_sol_cost, tok,
            &fee, &bc_info.creator, &bc_ata, &user_ata,
        ));
        if self.config.jito.enabled {
            ixs.push(self.jito_tip_ix(&payer));
        }
        self.simulate_and_send(keypair, &ixs).await
    }

    // ── Pump.fun sell ─────────────────────────────────────────────────────
    pub async fn pump_sell(
        &self,
        keypair:        &Keypair,
        mint:           &Pubkey,
        token_amount:   u64,
        min_sol_output: u64,
    ) -> Result<String> {
        let payer = keypair.pubkey();
        let fee   = self.fee_recip();

        let (bc_ata, tok) = self.find_bonding_curve_ata(mint).await?;
        let bc_info = pumpfun::fetch_bonding_curve_info(&self.rpc, mint).await?;

        tracing::info!("SELL {}… tokens={} min={:.4}SOL [{}] cashback={}",
            utils::short_key(mint), token_amount,
            utils::lamports_to_sol(min_sol_output), tok.label(),
            bc_info.cashback_enabled);

        let user_ata = utils::get_ata_with_program(&payer, mint, &tok.pubkey());

        let mut ixs = self.base_compute_ixs();
        ixs.push(pumpfun::build_sell_instruction(
            &payer, mint, token_amount, min_sol_output, tok,
            &fee, &bc_info.creator, &bc_ata, &user_ata,
            bc_info.cashback_enabled,
        ));
        if self.config.jito.enabled {
            ixs.push(self.jito_tip_ix(&payer));
        }
        self.simulate_and_send(keypair, &ixs).await
    }

    // ── Raydium swap (post-graduation) ────────────────────────────────────
    pub async fn raydium_swap(
        &self,
        keypair:        &Keypair,
        mint_str:       &str,
        amount_in:      u64,
        min_amount_out: u64,
        is_buy:         bool,
    ) -> Result<String> {
        let payer = keypair.pubkey();
        tracing::info!("RAYDIUM {} {}…", if is_buy {"BUY"} else {"SELL"}, &mint_str[..8.min(mint_str.len())]);
        let pool = raydium::fetch_pool_for_mint(&self.rpc, &self.http, mint_str).await?;
        let mint = Pubkey::from_str(mint_str)?;
        let wsol = Pubkey::from_str(raydium::SOL_MINT).unwrap();
        let (src, dst) = if is_buy {
            (utils::get_ata(&payer, &wsol), utils::get_ata(&payer, &mint))
        } else {
            (utils::get_ata(&payer, &mint), utils::get_ata(&payer, &wsol))
        };
        let mut ixs = self.base_compute_ixs();
        if is_buy {
            let spl = Pubkey::from_str(&spl_token::id().to_string()).unwrap();
            ixs.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &payer, &payer, &mint, &utils::to_spl_pubkey(&spl),
            ));
        }
        ixs.push(raydium::build_swap_instruction(&pool, &payer, &src, &dst, amount_in, min_amount_out));
        if self.config.jito.enabled {
            ixs.push(self.jito_tip_ix(&payer));
        }
        self.simulate_and_send(keypair, &ixs).await
    }

    // ── Token balance ─────────────────────────────────────────────────────
    pub async fn token_balance(&self, owner: &Pubkey, mint: &Pubkey) -> Result<u64> {
        for tok in [TokenProgram::Token2022, TokenProgram::Legacy] {
            let ata = utils::get_ata_with_program(owner, mint, &tok.pubkey());
            if let Ok(bal) = self.rpc.get_token_account_balance(&ata).await {
                let n = bal.amount.parse::<u64>().unwrap_or(0);
                if n > 0 { return Ok(n); }
            }
        }
        Ok(0)
    }

    pub async fn sol_balance(&self, w: &Pubkey) -> Result<u64> {
        self.rpc.get_balance(w).await.map_err(|e| anyhow!("get_balance: {}", e))
    }

    // ── Core: simulate → sign → Jito ─────────────────────────────────────
    pub async fn simulate_and_send(&self, keypair: &Keypair, ixs: &[Instruction]) -> Result<String> {
        let payer     = keypair.pubkey();
        let blockhash = self.rpc.get_latest_blockhash().await
            .map_err(|e| anyhow!("get_latest_blockhash: {}", e))?;
        let msg = Message::try_compile(&payer, ixs, &[], blockhash)
            .map_err(|e| anyhow!("compile: {}", e))?;
        let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[keypair])
            .map_err(|e| anyhow!("sign: {}", e))?;

        let sim = self.rpc.simulate_transaction(&tx).await
            .map_err(|e| anyhow!("simulate: {}", e))?;
        if let Some(err) = sim.value.err {
            let logs = sim.value.logs.unwrap_or_default();
            tracing::error!("SIMULATION FAILED: {:?}", err);
            for l in &logs { tracing::error!("  {}", l); }
            return Err(anyhow!("Simulation failed: {:?}\n{}", err, logs.join("\n")));
        }

        let sig    = tx.signatures[0].to_string();
        let raw    = bincode::serialize(&tx).map_err(|e| anyhow!("serialize: {}", e))?;
        let b58_tx = bs58::encode(&raw).into_string();

        if self.config.jito.enabled {
            self.send_jito_bundle(&b58_tx).await?;
        } else {
            self.rpc.send_transaction(&tx).await
                .map_err(|e| anyhow!("send_transaction: {}", e))?;
        }
        tracing::info!("TX: https://solscan.io/tx/{}", sig);
        Ok(sig)
    }

    async fn send_jito_bundle(&self, b58: &str) -> Result<()> {
        let p = json!({"jsonrpc":"2.0","id":1,"method":"sendBundle","params":[[b58]]});
        for url in JITO_REGIONS {
            if let Ok(r) = self.http.post(*url).json(&p).send().await {
                if let Ok(b) = r.json::<serde_json::Value>().await {
                    if b.get("result").is_some() {
                        tracing::info!("Bundle accepted by {}", url);
                        return Ok(());
                    }
                }
            }
        }
        tracing::warn!("All Jito regions failed — check Solscan");
        Ok(())
    }

    fn base_compute_ixs(&self) -> Vec<Instruction> {
        vec![
            ComputeBudgetInstruction::set_compute_unit_limit(300_000),
            ComputeBudgetInstruction::set_compute_unit_price(self.config.strategy.priority_fee_micro_lamports),
        ]
    }

    fn jito_tip_ix(&self, payer: &Pubkey) -> Instruction {
        let idx  = (chrono::Utc::now().timestamp() as usize) % JITO_TIP_ACCOUNTS.len();
        let acct = JITO_TIP_ACCOUNTS.iter().cycle().skip(idx)
            .find_map(|s| Pubkey::from_str(s).ok()).expect("invalid jito tip");
        system_instruction::transfer(payer, &acct, self.config.jito.tip_lamports)
    }
}
