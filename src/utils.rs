// src/utils.rs

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

// ─────────────────────────────────────────────────────────────────────────────
//  ATA derivation
// ─────────────────────────────────────────────────────────────────────────────

/// Legacy SPL token ATA — same as Phase 1, unchanged.
pub fn get_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let spl_wallet = to_spl_pubkey(wallet);
    let spl_mint   = to_spl_pubkey(mint);
    let ata = spl_associated_token_account::get_associated_token_address(&spl_wallet, &spl_mint);
    from_spl_pubkey(&ata)
}

/// Phase 2: ATA for ANY token program (legacy SPL or Token-2022).
/// Pass the token_program pubkey detected at runtime from the mint account.
pub fn get_ata_with_program(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let spl_wallet  = to_spl_pubkey(wallet);
    let spl_mint    = to_spl_pubkey(mint);
    let spl_tok_pid = to_spl_pubkey(token_program);
    let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        &spl_wallet,
        &spl_mint,
        &spl_tok_pid,
    );
    from_spl_pubkey(&ata)
}

/// solana_sdk::Pubkey → spl_token::solana_program::Pubkey
pub fn to_spl_pubkey(pk: &Pubkey) -> spl_token::solana_program::pubkey::Pubkey {
    spl_token::solana_program::pubkey::Pubkey::from_str(&pk.to_string())
        .expect("pubkey round-trip failed")
}

/// spl_token::solana_program::Pubkey → solana_sdk::Pubkey
pub fn from_spl_pubkey(pk: &spl_token::solana_program::pubkey::Pubkey) -> Pubkey {
    Pubkey::from_str(&pk.to_string()).expect("pubkey round-trip failed")
}

// ─────────────────────────────────────────────────────────────────────────────
//  Formatting helpers
// ─────────────────────────────────────────────────────────────────────────────

pub fn lamports_to_sol(lamports: u64) -> f64 {
    lamports as f64 / 1_000_000_000.0
}

pub fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1_000_000_000.0) as u64
}

pub fn short_key(pk: &Pubkey) -> String {
    let s = pk.to_string();
    format!("{}…{}", &s[..4], &s[s.len() - 4..])
}

// ─────────────────────────────────────────────────────────────────────────────
//  Retry helper
// ─────────────────────────────────────────────────────────────────────────────

pub async fn retry_async<F, Fut, T>(max_attempts: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = anyhow::anyhow!("retry_async called with 0 attempts");
    for attempt in 1..=max_attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = e;
                if attempt < max_attempts {
                    let backoff = 200u64 * (1 << (attempt - 1).min(4));
                    tracing::warn!(
                        "Attempt {}/{} failed, retrying in {}ms…",
                        attempt, max_attempts, backoff
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                }
            }
        }
    }
    Err(last_err)
}
