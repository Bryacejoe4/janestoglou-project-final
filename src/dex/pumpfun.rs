// src/dex/pumpfun.rs — DEFINITIVE (May 2026)
// All addresses verified from real on-chain transactions.
// fee_config and global_volume_accumulator are hardcoded (not derived) — confirmed from tx data.
// Trailing fee recipients (index 17) verified from multiple live buy transactions.

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

// Hardcoded from real tx data — index 14 in every buy instruction
pub const FEE_CONFIG: &str = "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt";

// Hardcoded from real tx data — index 12 in every buy instruction
pub const GLOBAL_VOLUME_ACCUMULATOR: &str = "Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y";

// Authorized trailing fee recipients (index 17) — verified from real buy txs
pub const TRAILING_FEE_RECIPIENTS: [&str; 3] = [
    "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL",
    "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6",
    "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
];

const BUY_DISCRIMINATOR:  [u8; 8] = [102,   6,  61,  18,   1, 218, 235, 234];
const SELL_DISCRIMINATOR: [u8; 8] = [ 51, 230, 133, 164,   1, 127, 131, 173];

// ── Token program ──────────────────────────────────────────────────────────
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

// ── Bonding curve info ─────────────────────────────────────────────────────
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
    let creator = if data.len() >= 81 {
        let bytes: [u8; 32] = data[49..81].try_into()
            .map_err(|_| anyhow!("creator slice error"))?;
        Pubkey::new_from_array(bytes)
    } else {
        Pubkey::default()
    };
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

// ── PDAs ───────────────────────────────────────────────────────────────────
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
    // Hardcoded from real tx data — derivation was unreliable
    Pubkey::from_str(GLOBAL_VOLUME_ACCUMULATOR).unwrap()
}
pub fn user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &program_id()).0
}
pub fn fee_config_pda() -> Pubkey {
    // Hardcoded from real tx data — PDA derivation produced wrong address
    Pubkey::from_str(FEE_CONFIG).unwrap()
}

// Pick an authorized trailing fee recipient — rotate to spread load
pub fn pick_trailing_fee_recipient() -> Pubkey {
    let idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize) % TRAILING_FEE_RECIPIENTS.len();
    Pubkey::from_str(TRAILING_FEE_RECIPIENTS[idx]).unwrap()
}

// ── Buy instruction — 18 accounts ─────────────────────────────────────────
// Verified layout from real on-chain buy transactions (May 2026):
//  0  global                    readonly
//  1  fee_recipient             writable
//  2  mint                      readonly
//  3  bonding_curve             writable
//  4  associated_bonding_curve  writable
//  5  associated_user           writable
//  6  user/payer                signer writable
//  7  system_program            readonly
//  8  token_program             readonly
//  9  creator_vault             writable
// 10  event_authority           readonly
// 11  program                   readonly
// 12  global_volume_accumulator readonly (hardcoded)
// 13  user_volume_accumulator   writable
// 14  fee_config                readonly (hardcoded)
// 15  fee_program               readonly
// 16  bonding_curve_v2          writable
// 17  trailing_fee_recipient    writable (authorized list)
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
    let pid      = program_id();
    let ev       = Pubkey::from_str(EVENT_AUTHORITY).unwrap();
    let gva      = global_volume_accumulator_pda();   // hardcoded
    let uva      = user_volume_accumulator_pda(payer);
    let cv       = creator_vault_pda(creator);
    let fpid     = fee_program_id();
    let fcfg     = fee_config_pda();                  // hardcoded
    let bcv2     = bonding_curve_v2_pda(mint);
    let trailing = pick_trailing_fee_recipient();

    let mut data = BUY_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    Instruction {
        program_id: pid,
        accounts: vec![
            AccountMeta::new_readonly(global_pda(),         false), // 0
            AccountMeta::new(*fee_recipient,                 false), // 1
            AccountMeta::new_readonly(*mint,                 false), // 2
            AccountMeta::new(bonding_curve_pda(mint),        false), // 3
            AccountMeta::new(*bc_ata,                        false), // 4
            AccountMeta::new(*user_ata,                      false), // 5
            AccountMeta::new(*payer,                         true),  // 6
            AccountMeta::new_readonly(system_program::id(),  false), // 7
            AccountMeta::new_readonly(tok.pubkey(),          false), // 8
            AccountMeta::new(cv,                             false), // 9
            AccountMeta::new_readonly(ev,                    false), // 10
            AccountMeta::new_readonly(pid,                   false), // 11
            AccountMeta::new_readonly(gva,                   false), // 12
            AccountMeta::new(uva,                            false), // 13
            AccountMeta::new_readonly(fcfg,                  false), // 14
            AccountMeta::new_readonly(fpid,                  false), // 15
            AccountMeta::new(bcv2,                           false), // 16
            AccountMeta::new(trailing,                       false), // 17
        ],
        data,
    }
}

// ── Sell instruction ───────────────────────────────────────────────────────
// Non-cashback: 16 accounts. Cashback: 17 accounts (uva before bcv2).
// Same trailing fee recipient added as last account.
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
    let ev       = Pubkey::from_str(EVENT_AUTHORITY).unwrap();
    let cv       = creator_vault_pda(creator);
    let fcfg     = fee_config_pda();
    let bcv2     = bonding_curve_v2_pda(mint);
    let trailing = pick_trailing_fee_recipient();

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
        AccountMeta::new(cv,                             false), // 8
        AccountMeta::new_readonly(tok.pubkey(),          false), // 9
        AccountMeta::new_readonly(ev,                    false), // 10
        AccountMeta::new_readonly(pid,                   false), // 11
        AccountMeta::new_readonly(fcfg,                  false), // 12
        AccountMeta::new_readonly(fee_program_id(),      false), // 13
    ];

    if cashback_enabled {
        accounts.push(AccountMeta::new(user_volume_accumulator_pda(payer), false)); // 14
    }
    accounts.push(AccountMeta::new(bcv2,     false)); // 14 or 15
    accounts.push(AccountMeta::new(trailing, false)); // 15 or 16

    Instruction { program_id: pid, accounts, data }
}
