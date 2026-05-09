// src/dex/pumpfun.rs
// Token-2022 detection uses a hardcoded verified constant — NOT the spl-token-2022 crate.
// The crate conflicts with solana-sdk 1.18 and returns wrong program IDs at runtime.

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program, sysvar,
};
use std::str::FromStr;

// ── Program constants ─────────────────────────────────────────────────────
pub const PUMP_PROGRAM_ID:        &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const EVENT_AUTHORITY:        &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7jxXpXhH";
pub const FEE_RECIPIENT_FALLBACK: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";

// ── Token program IDs — VERIFIED on-chain addresses ──────────────────────
// Legacy SPL token program (most tokens including all Pump.fun tokens)
pub const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
// Token-2022 program (newer tokens only — rare on Pump.fun)
// Verified: https://spl.solana.com/token-2022
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// Anchor discriminators
const BUY_DISCRIMINATOR:  [u8; 8] = [102,   6,  61,  18,   1, 218, 235, 234];
const SELL_DISCRIMINATOR: [u8; 8] = [ 51, 230, 133, 164,   1, 127, 131, 173];

// ─────────────────────────────────────────────────────────────────────────────
//  TokenProgram
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenProgram {
    Legacy,
    Token2022,
}

impl TokenProgram {
    pub fn pubkey(&self) -> Pubkey {
        match self {
            // Use spl_token::id() for legacy — known to be correct with solana-sdk 1.18
            Self::Legacy => Pubkey::from_str(&spl_token::id().to_string()).unwrap(),
            // Use the verified hardcoded string — NOT spl_token_2022::id() which conflicts
            Self::Token2022 => Pubkey::from_str(TOKEN_2022_PROGRAM_ID).unwrap(),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Legacy    => "legacy-SPL",
            Self::Token2022 => "Token-2022",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Runtime detection — check mint account owner against KNOWN constants
// ─────────────────────────────────────────────────────────────────────────────

pub async fn detect_token_program(rpc: &RpcClient, mint: &Pubkey) -> TokenProgram {
    match rpc.get_account(mint).await {
        Ok(account) => {
            let owner = account.owner.to_string();
            if owner == TOKEN_2022_PROGRAM_ID {
                tracing::debug!("{}… → Token-2022", &mint.to_string()[..8]);
                TokenProgram::Token2022
            } else if owner == SPL_TOKEN_PROGRAM_ID {
                tracing::debug!("{}… → legacy SPL", &mint.to_string()[..8]);
                TokenProgram::Legacy
            } else {
                // Unknown program — default to legacy, log it so we can investigate
                tracing::warn!(
                    "{}… has unknown token program: {} — defaulting to legacy",
                    &mint.to_string()[..8], owner
                );
                TokenProgram::Legacy
            }
        }
        Err(e) => {
            tracing::warn!(
                "detect_token_program for {}… failed: {} — defaulting to legacy",
                &mint.to_string()[..8], e
            );
            TokenProgram::Legacy
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Dynamic fee recipient
// ─────────────────────────────────────────────────────────────────────────────

pub async fn fetch_fee_recipient(rpc: &RpcClient) -> Result<Pubkey> {
    let data = rpc.get_account_data(&global_pda()).await
        .map_err(|e| anyhow!("fetch_fee_recipient RPC: {}", e))?;

    if data.len() < 73 {
        return Err(anyhow!("global account too short ({} bytes)", data.len()));
    }

    let bytes: [u8; 32] = data[41..73]
        .try_into()
        .map_err(|_| anyhow!("fee_recipient slice error"))?;

    Ok(Pubkey::new_from_array(bytes))
}

// ─────────────────────────────────────────────────────────────────────────────
//  PDAs
// ─────────────────────────────────────────────────────────────────────────────

pub fn program_id() -> Pubkey { Pubkey::from_str(PUMP_PROGRAM_ID).unwrap() }

pub fn global_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global"], &program_id()).0
}

pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &program_id()).0
}


pub fn associated_bonding_curve(mint: &Pubkey) -> Pubkey {
    let legacy = Pubkey::from_str(SPL_TOKEN_PROGRAM_ID).unwrap();
    crate::utils::get_ata_with_program(&bonding_curve_pda(mint), mint, &legacy)
}

// ─────────────────────────────────────────────────────────────────────────────
//  Buy instruction
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_buy_instruction(
    payer:         &Pubkey,
    mint:          &Pubkey,
    token_amount:  u64,
    max_sol_cost:  u64,
    tok:           TokenProgram,
    fee_recipient: &Pubkey,
) -> Instruction {
    let pid      = program_id();
    let user_ata = crate::utils::get_ata_with_program(payer, mint, &tok.pubkey());

    let mut data = BUY_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    Instruction {
        program_id: pid,
        accounts: vec![
            AccountMeta::new_readonly(global_pda(),                                   false),
            AccountMeta::new(*fee_recipient,                                           false),
            AccountMeta::new_readonly(*mint,                                           false),
            AccountMeta::new(bonding_curve_pda(mint),                                  false),
            AccountMeta::new(associated_bonding_curve(mint),                      false),
            AccountMeta::new(user_ata,                                                 false),
            AccountMeta::new(*payer,                                                   true),
            AccountMeta::new_readonly(system_program::id(),                            false),
            AccountMeta::new_readonly(tok.pubkey(),                                    false),
            AccountMeta::new_readonly(sysvar::rent::id(),                              false),
            AccountMeta::new_readonly(Pubkey::from_str(EVENT_AUTHORITY).unwrap(),      false),
            AccountMeta::new_readonly(pid,                                             false),
        ],
        data,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Sell instruction
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_sell_instruction(
    payer:          &Pubkey,
    mint:           &Pubkey,
    token_amount:   u64,
    min_sol_output: u64,
    tok:            TokenProgram,
    fee_recipient:  &Pubkey,
) -> Instruction {
    let pid      = program_id();
    let user_ata = crate::utils::get_ata_with_program(payer, mint, &tok.pubkey());

    let mut data = SELL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    Instruction {
        program_id: pid,
        accounts: vec![
            AccountMeta::new_readonly(global_pda(),                                   false),
            AccountMeta::new(*fee_recipient,                                           false),
            AccountMeta::new_readonly(*mint,                                           false),
            AccountMeta::new(bonding_curve_pda(mint),                                  false),
            AccountMeta::new(associated_bonding_curve(mint),                      false),
            AccountMeta::new(user_ata,                                                 false),
            AccountMeta::new(*payer,                                                   true),
            AccountMeta::new_readonly(system_program::id(),                            false),
            AccountMeta::new_readonly(tok.pubkey(),                                    false),
            AccountMeta::new_readonly(sysvar::rent::id(),                              false),
            AccountMeta::new_readonly(Pubkey::from_str(EVENT_AUTHORITY).unwrap(),      false),
            AccountMeta::new_readonly(pid,                                             false),
        ],
        data,
    }
}
/*
            AccountMeta::new(*payer,                                                   true),
            AccountMeta::new_readonly(system_program::id(),                            false),
            AccountMeta::new_readonly(tok.pubkey(),                                    false),
            AccountMeta::new_readonly(sysvar::rent::id(),                              false),
            AccountMeta::new_readonly(Pubkey::from_str(EVENT_AUTHORITY).unwrap(),      false),
            AccountMeta::new_readonly(pid,                                             false),
        ],
        data,
    }
}
*/