// src/dex/pumpfun.rs
// DEFINITIVE — based on official Pump.fun developer Telegram + AllenHark March 2026
//
// Timeline of breaking changes:
// Aug 1 2025:  Added global_volume_accumulator (idx 12) + user_volume_accumulator (idx 13) to BUY
// Sep 2025:    Added fee_config (idx 14) + fee_program (idx 15) to BUY and SELL
// Feb 2026:    Cashback upgrade:
//              - rent account REMOVED, creator_vault inserted at idx 9
//              - fee_program account removed, bonding_curve_v2 added as last account
//              - cashback_enabled flag at bonding_curve byte[82]
//              - For cashback SELL only: user_volume_accumulator inserted before bonding_curve_v2
//
// CURRENT LAYOUT (March 2026):
//   BUY non-cashback: 16 accounts (0-15)
//   BUY cashback:     same 16 accounts (cashback only affects sell)
//   SELL non-cashback: 14 accounts
//   SELL cashback:     15 accounts (user_volume_accumulator before bonding_curve_v2)
//
// fee_config PDA seeds: ["fee_config", PUMP_PROGRAM_ID_bytes] using fee_program
// Source: https://t.me/pump_tech_updates + https://allenhark.com/blog/pumpfun-bonding-curve-custom-6024-overflow-fix-cashback-upgrade-guide

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use std::str::FromStr;

pub const PUMP_PROGRAM_ID:        &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const EVENT_AUTHORITY:        &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
pub const FEE_RECIPIENT_FALLBACK: &str = "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV";
pub const FEE_PROGRAM_ID:         &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
pub const SPL_TOKEN_PROGRAM_ID:   &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM_ID:  &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

const BUY_DISCRIMINATOR:  [u8; 8] = [102,   6,  61,  18,   1, 218, 235, 234];
const SELL_DISCRIMINATOR: [u8; 8] = [ 51, 230, 133, 164,   1, 127, 131, 173];

// ── Token program ─────────────────────────────────────────────────────────
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

pub async fn detect_token_program(rpc: &RpcClient, mint: &Pubkey) -> TokenProgram {
    match rpc.get_account(mint).await {
        Ok(a) if a.owner.to_string() == TOKEN_2022_PROGRAM_ID => TokenProgram::Token2022,
        _ => TokenProgram::Legacy,
    }
}

// ── Bonding curve info ────────────────────────────────────────────────────
// Layout (151 bytes after Feb 2026 cashback upgrade):
// [8]  discriminator
// [8]  virtual_token_reserves
// [8]  virtual_sol_reserves
// [8]  real_token_reserves
// [8]  real_sol_reserves
// [8]  token_total_supply
// [1]  complete               ← offset 48
// [32] creator                ← offset 49
// [1]  (reserved)
// [1]  cashback_enabled       ← offset 82

pub struct BondingCurveInfo {
    pub creator:          Pubkey,
    pub cashback_enabled: bool,
    pub complete:         bool,
}

pub async fn fetch_bonding_curve_info(rpc: &RpcClient, mint: &Pubkey) -> Result<BondingCurveInfo> {
    let data = rpc.get_account_data(&bonding_curve_pda(mint)).await
        .map_err(|e| anyhow!("fetch bonding curve: {}", e))?;
    if data.len() < 49 { return Err(anyhow!("bonding curve too short: {}", data.len())); }

    let complete = data[48] != 0;

    // creator is present if data is >= 81 bytes
    let creator = if data.len() >= 81 {
        let bytes: [u8; 32] = data[49..81].try_into()
            .map_err(|_| anyhow!("creator slice error"))?;
        Pubkey::new_from_array(bytes)
    } else {
        Pubkey::default()
    };

    // cashback_enabled at byte 82 (only present in newer bonding curves)
    let cashback_enabled = data.len() > 82 && data[82] != 0;

    Ok(BondingCurveInfo { creator, cashback_enabled, complete })
}

pub async fn fetch_fee_recipient(rpc: &RpcClient) -> Result<Pubkey> {
    let data = rpc.get_account_data(&global_pda()).await
        .map_err(|e| anyhow!("fetch_fee_recipient: {}", e))?;
    if data.len() < 73 { return Err(anyhow!("global account too short")); }
    let bytes: [u8; 32] = data[41..73].try_into().map_err(|_| anyhow!("slice error"))?;
    Ok(Pubkey::new_from_array(bytes))
}

// ── PDAs ──────────────────────────────────────────────────────────────────
pub fn program_id()     -> Pubkey { Pubkey::from_str(PUMP_PROGRAM_ID).unwrap() }
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

/// fee_config PDA — official seeds from Pump.fun developer Telegram:
/// seeds = ["fee_config", PUMP_PROGRAM_ID_bytes], program = fee_program
pub fn fee_config_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[b"fee_config", program_id().as_ref()],   // two seeds: "fee_config" + pump program
        &fee_program_id(),
    ).0
}

// ── Buy instruction — 16 accounts ────────────────────────────────────────
// Note: rent sysvar was REMOVED in Feb 2026; creator_vault now at idx 9
pub fn build_buy_instruction(
    payer:         &Pubkey,
    mint:          &Pubkey,
    token_amount:  u64,
    max_sol_cost:  u64,
    tok:           TokenProgram,
    fee_recipient: &Pubkey,
    creator:       &Pubkey,
    bc_ata:        &Pubkey,
    user_ata:      &Pubkey,
) -> Instruction {
    let pid  = program_id();
    let ev   = Pubkey::from_str(EVENT_AUTHORITY).unwrap();
    let gva  = global_volume_accumulator_pda();
    let uva  = user_volume_accumulator_pda(payer);
    let cv   = creator_vault_pda(creator);
    let fpid = fee_program_id();
    let fcfg = fee_config_pda();
    let bcv2 = bonding_curve_v2_pda(mint);

    let mut data = BUY_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    Instruction {
        program_id: pid,
        accounts: vec![
            AccountMeta::new_readonly(global_pda(),          false), // 0  global
            AccountMeta::new(*fee_recipient,                 false), // 1  fee_recipient
            AccountMeta::new_readonly(*mint,                 false), // 2  mint
            AccountMeta::new(bonding_curve_pda(mint),        false), // 3  bonding_curve
            AccountMeta::new(*bc_ata,                        false), // 4  associated_bonding_curve
            AccountMeta::new(*user_ata,                      false), // 5  associated_user
            AccountMeta::new(*payer,                         true),  // 6  user (signer)
            AccountMeta::new_readonly(system_program::id(),  false), // 7  system_program
            AccountMeta::new_readonly(tok.pubkey(),          false), // 8  token_program
            AccountMeta::new(cv,                             false), // 9  creator_vault (replaces rent)
            AccountMeta::new_readonly(ev,                    false), // 10 event_authority
            AccountMeta::new_readonly(pid,                   false), // 11 program
            AccountMeta::new(gva,                            false), // 12 global_volume_accumulator
            AccountMeta::new(uva,                            false), // 13 user_volume_accumulator
            AccountMeta::new_readonly(fcfg,                  false), // 14 fee_config  ← PDA first
            AccountMeta::new_readonly(fpid,                  false), // 15 fee_program ← program ID second
            AccountMeta::new(bcv2,                           false), // 16 bonding_curve_v2
        ],
        data,
    }
}

// ── Sell instruction — 14 or 15 accounts ─────────────────────────────────
// cashback sell (15): insert user_volume_accumulator before bonding_curve_v2
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
    let pid  = program_id();
    let ev   = Pubkey::from_str(EVENT_AUTHORITY).unwrap();
    let cv   = creator_vault_pda(creator);
    let fcfg = fee_config_pda();
    let bcv2 = bonding_curve_v2_pda(mint);

    let mut data = SELL_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new_readonly(global_pda(),         false), // 0
        AccountMeta::new(*fee_recipient,                 false), // 1
        AccountMeta::new_readonly(*mint,                 false), // 2
        AccountMeta::new(bonding_curve_pda(mint),        false), // 3
        AccountMeta::new(*bc_ata,                        false), // 4
        AccountMeta::new(*user_ata,                      false), // 5
        AccountMeta::new(*payer,                         true),  // 6
        AccountMeta::new_readonly(system_program::id(),  false), // 7
        AccountMeta::new_readonly(tok.pubkey(),          false), // 8
        AccountMeta::new(cv,                             false), // 9  creator_vault
        AccountMeta::new_readonly(ev,                    false), // 10
        AccountMeta::new_readonly(pid,                   false), // 11
        AccountMeta::new_readonly(fcfg,                  false), // 12 fee_config
    ];

    // cashback sell: add user_volume_accumulator before bonding_curve_v2
    if cashback_enabled {
        let uva = user_volume_accumulator_pda(payer);
        accounts.push(AccountMeta::new(uva, false)); // 13 (cashback only)
    }

    accounts.push(AccountMeta::new(bcv2, false)); // 13 or 14 — always last

    Instruction { program_id: pid, accounts, data }
}
