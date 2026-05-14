// src/dex/pumpfun.rs
// COMPLETE FIX — updated for all Pump.fun program changes through April 2026:
//
// Aug 2025:  +global_volume_accumulator (idx 12), +user_volume_accumulator (idx 13)
// Feb 2026:  +creator_vault (idx 9 replacing rent), +fee_config (idx 14),
//             +fee_program (idx 15), +bonding_curve_v2 (idx 16)
// Apr 2026:  +trailing_fee_recipient (idx 17) — 18 accounts total for buy
//
// Sell is different: 15 accounts (non-cashback) or 16 accounts (cashback).
// cashback_enabled is read from bonding curve data byte[82].

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use std::str::FromStr;

pub const PUMP_PROGRAM_ID:      &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const FEE_PROGRAM_ID:       &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
pub const EVENT_AUTHORITY:      &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
pub const FEE_RECIPIENT_FALLBACK: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
pub const SPL_TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// 8 valid fee recipients (any one can be used — use first as default)
pub const TRAILING_FEE_RECIPIENT: &str = "GesfTA3X2arioaHp8bbKdjG9vJtskViWACZoYvxp4twS";

// fee_config PDA key (hardcoded by Pump.fun protocol)
const FEE_CONFIG_KEY: [u8; 32] = [
    1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170,
    81, 137, 203, 151, 245, 210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176,
];

const BUY_DISCRIMINATOR:  [u8; 8] = [102,   6,  61,  18,   1, 218, 235, 234];
const SELL_DISCRIMINATOR: [u8; 8] = [ 51, 230, 133, 164,   1, 127, 131, 173];

// ─────────────────────────────────────────────────────────────────────────────
//  Token program
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TokenProgram { Legacy, Token2022 }

impl TokenProgram {
    pub fn pubkey(&self) -> Pubkey {
        match self {
            Self::Legacy    => Pubkey::from_str(SPL_TOKEN_PROGRAM_ID).unwrap(),
            Self::Token2022 => Pubkey::from_str(TOKEN_2022_PROGRAM_ID).unwrap(),
        }
    }
    pub fn label(&self) -> &'static str {
        match self { Self::Legacy => "legacy-SPL", Self::Token2022 => "Token-2022" }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Bonding curve data — parsed from on-chain account
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct BondingCurveData {
    pub creator:          Pubkey,
    pub cashback_enabled: bool,
    pub complete:         bool,
}

pub async fn fetch_bonding_curve_data(rpc: &RpcClient, mint: &Pubkey) -> Result<BondingCurveData> {
    let bc   = bonding_curve_pda(mint);
    let data = rpc.get_account_data(&bc).await
        .map_err(|e| anyhow!("fetch bonding curve: {}", e))?;

    if data.len() < 83 {
        return Err(anyhow!("bonding curve data too short: {} bytes", data.len()));
    }

    // Layout after 8-byte discriminator:
    // +0  virtual_token_reserves u64
    // +8  virtual_sol_reserves   u64
    // +16 real_token_reserves    u64
    // +24 real_sol_reserves      u64
    // +32 token_total_supply     u64
    // +40 complete               bool
    // +41 creator                Pubkey (32 bytes)
    // +73 (reserved)             u8
    // +74 cashback_enabled       bool   ← NOTE: article says byte[82] = offset 82 from start of data
    //                                          = offset 74 from after discriminator

    let complete = data[48] != 0;  // byte 48 = discriminator(8) + 40 = complete

    let creator_bytes: [u8; 32] = data[49..81].try_into()
        .map_err(|_| anyhow!("creator slice error"))?;
    let creator = Pubkey::new_from_array(creator_bytes);

    // cashback_enabled is at byte offset 82 from start of data (including discriminator)
    let cashback_enabled = data.len() > 82 && data[82] != 0;

    Ok(BondingCurveData { creator, cashback_enabled, complete })
}

// Also keep detect_token_program for backward compatibility
pub async fn detect_token_program(rpc: &RpcClient, mint: &Pubkey) -> TokenProgram {
    match rpc.get_account(mint).await {
        Ok(account) => {
            if account.owner.to_string() == TOKEN_2022_PROGRAM_ID {
                tracing::debug!("{}… → Token-2022", &mint.to_string()[..8]);
                TokenProgram::Token2022
            } else {
                tracing::debug!("{}… → legacy SPL", &mint.to_string()[..8]);
                TokenProgram::Legacy
            }
        }
        Err(_) => TokenProgram::Legacy,
    }
}

pub async fn fetch_fee_recipient(rpc: &RpcClient) -> Result<Pubkey> {
    let data = rpc.get_account_data(&global_pda()).await
        .map_err(|e| anyhow!("fetch_fee_recipient RPC: {}", e))?;
    if data.len() < 73 { return Err(anyhow!("global account too short")); }
    let bytes: [u8; 32] = data[41..73].try_into().map_err(|_| anyhow!("slice error"))?;
    Ok(Pubkey::new_from_array(bytes))
}

// ─────────────────────────────────────────────────────────────────────────────
//  PDAs
// ─────────────────────────────────────────────────────────────────────────────

pub fn program_id() -> Pubkey { Pubkey::from_str(PUMP_PROGRAM_ID).unwrap() }
pub fn fee_program_id() -> Pubkey { Pubkey::from_str(FEE_PROGRAM_ID).unwrap() }

pub fn global_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global"], &program_id()).0
}
pub fn bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &program_id()).0
}
pub fn bonding_curve_v2_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve-v2", mint.as_ref()], &program_id()).0
}
pub fn creator_vault_pda(creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &program_id()).0
}
pub fn global_volume_accumulator_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &program_id()).0
}
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &program_id()).0
}
pub fn fee_config_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"fee_config", &FEE_CONFIG_KEY], &fee_program_id()).0
}

// ─────────────────────────────────────────────────────────────────────────────
//  Buy instruction — 18 accounts (as of April 2026)
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_buy_instruction(
    payer:         &Pubkey,
    mint:          &Pubkey,
    token_amount:  u64,
    max_sol_cost:  u64,
    tok:           TokenProgram,
    fee_recipient: &Pubkey,
    creator:       &Pubkey,
    bc_ata:        &Pubkey,   // probed from RPC
    user_ata:      &Pubkey,   // user's ATA
) -> Instruction {
    let pid      = program_id();
    let gva      = global_volume_accumulator_pda();
    let uva      = user_volume_accumulator_pda(payer);
    let fee_cfg  = fee_config_pda();
    let fee_prog = fee_program_id();
    let bc_v2    = bonding_curve_v2_pda(mint);
    let cr_vault = creator_vault_pda(creator);
    let ev_auth  = Pubkey::from_str(EVENT_AUTHORITY).unwrap();
    let trailing = Pubkey::from_str(TRAILING_FEE_RECIPIENT).unwrap();

    let mut data = BUY_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    Instruction {
        program_id: pid,
        accounts: vec![
            AccountMeta::new_readonly(global_pda(),          false), // 0  global
            AccountMeta::new(*fee_recipient,                  false), // 1  fee_recipient (writable)
            AccountMeta::new_readonly(*mint,                  false), // 2  mint
            AccountMeta::new(bonding_curve_pda(mint),         false), // 3  bonding_curve
            AccountMeta::new(*bc_ata,                         false), // 4  associated_bonding_curve
            AccountMeta::new(*user_ata,                       false), // 5  associated_user
            AccountMeta::new(*payer,                          true),  // 6  user (signer)
            AccountMeta::new_readonly(system_program::id(),   false), // 7  system_program
            AccountMeta::new_readonly(tok.pubkey(),           false), // 8  token_program
            AccountMeta::new(cr_vault,                        false), // 9  creator_vault
            AccountMeta::new_readonly(ev_auth,                false), // 10 event_authority
            AccountMeta::new_readonly(pid,                    false), // 11 program
            AccountMeta::new_readonly(gva,                    false), // 12 global_volume_accumulator
            AccountMeta::new(uva,                             false), // 13 user_volume_accumulator
            AccountMeta::new_readonly(fee_cfg,                false), // 14 fee_config
            AccountMeta::new_readonly(fee_prog,               false), // 15 fee_program
            AccountMeta::new_readonly(bc_v2,                  false), // 16 bonding_curve_v2
            AccountMeta::new(trailing,                        false), // 17 trailing fee recipient
        ],
        data,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Sell instruction — 15 accounts (non-cashback) or 16 (cashback)
//  NOTE: token_program is at index 9 in sell (different from buy!)
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_sell_instruction(
    payer:            &Pubkey,
    mint:             &Pubkey,
    token_amount:     u64,
    min_sol_output:   u64,
    tok:              TokenProgram,
    fee_recipient:    &Pubkey,
    creator:          &Pubkey,
    bc_ata:           &Pubkey,
    user_ata:         &Pubkey,
    cashback_enabled: bool,
) -> Instruction {
    let pid      = program_id();
    let bc_v2    = bonding_curve_v2_pda(mint);
    let cr_vault = creator_vault_pda(creator);
    let ev_auth  = Pubkey::from_str(EVENT_AUTHORITY).unwrap();
    let fee_cfg  = fee_config_pda();
    let fee_prog = fee_program_id();

    let mut data = SELL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new_readonly(global_pda(),        false), // 0
        AccountMeta::new(*fee_recipient,                false), // 1
        AccountMeta::new_readonly(*mint,                false), // 2
        AccountMeta::new(bonding_curve_pda(mint),       false), // 3
        AccountMeta::new(*bc_ata,                       false), // 4
        AccountMeta::new(*user_ata,                     false), // 5
        AccountMeta::new(*payer,                        true),  // 6
        AccountMeta::new_readonly(system_program::id(), false), // 7
        AccountMeta::new(cr_vault,                      false), // 8  creator_vault
        AccountMeta::new_readonly(tok.pubkey(),         false), // 9  token_program (after creator_vault!)
        AccountMeta::new_readonly(ev_auth,              false), // 10
        AccountMeta::new_readonly(pid,                  false), // 11
        AccountMeta::new_readonly(fee_cfg,              false), // 12
        AccountMeta::new_readonly(fee_prog,             false), // 13
    ];

    // Cashback tokens get user_volume_accumulator before bonding_curve_v2
    if cashback_enabled {
        let uva = user_volume_accumulator_pda(payer);
        accounts.push(AccountMeta::new(uva, false)); // 14
    }

    accounts.push(AccountMeta::new_readonly(bc_v2, false)); // 14 or 15 — always last

    Instruction { program_id: pid, accounts, data }
}
