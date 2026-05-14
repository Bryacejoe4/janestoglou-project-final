// src/utils.rs

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub fn get_ata(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let sw = to_spl(wallet);
    let sm = to_spl(mint);
    from_spl(&spl_associated_token_account::get_associated_token_address(&sw, &sm))
}

pub fn get_ata_with_program(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let sw = to_spl(wallet);
    let sm = to_spl(mint);
    let sp = to_spl(token_program);
    from_spl(&spl_associated_token_account::get_associated_token_address_with_program_id(&sw, &sm, &sp))
}

/// solana_sdk::Pubkey → spl_token::solana_program::Pubkey
/// Called both as to_spl() and to_spl_pubkey() — both names work
pub fn to_spl(pk: &Pubkey) -> spl_token::solana_program::pubkey::Pubkey {
    spl_token::solana_program::pubkey::Pubkey::from_str(&pk.to_string())
        .expect("pubkey round-trip failed")
}

/// Alias so existing code calling to_spl_pubkey() still compiles
pub fn to_spl_pubkey(pk: &Pubkey) -> spl_token::solana_program::pubkey::Pubkey {
    to_spl(pk)
}

pub fn from_spl(pk: &spl_token::solana_program::pubkey::Pubkey) -> Pubkey {
    Pubkey::from_str(&pk.to_string()).expect("pubkey round-trip failed")
}

pub fn lamports_to_sol(lamports: u64) -> f64 { lamports as f64 / 1_000_000_000.0 }
pub fn sol_to_lamports(sol: f64) -> u64 { (sol * 1_000_000_000.0) as u64 }

pub fn short_key(pk: &Pubkey) -> String {
    let s = pk.to_string();
    format!("{}…{}", &s[..4], &s[s.len()-4..])
}

pub async fn retry_async<F, Fut, T>(max_attempts: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = anyhow::anyhow!("retry_async: 0 attempts");
    for attempt in 1..=max_attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = e;
                if attempt < max_attempts {
                    let backoff = 200u64 * (1 << (attempt - 1).min(4));
                    tracing::warn!("Attempt {}/{} failed, retrying in {}ms…", attempt, max_attempts, backoff);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                }
            }
        }
    }
    Err(last_err)
}
