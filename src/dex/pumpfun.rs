// src/dex/pumpfun.rs
// DEFINITIVE VERSION — fixes ProgramAccountNotFound once and for all.
//
// Root cause of ProgramAccountNotFound:
//   We detected the token program from the mint account owner, then built
//   the bonding curve ATA using that program. But Pump.fun ALWAYS uses the
//   legacy SPL token program for the bonding curve ATA — regardless of what
//   program owns the mint. Using Token-2022 for the bonding curve ATA produces
//   an account address that does not exist on-chain → ProgramAccountNotFound.
//
// Fix: bonding curve ATA is ALWAYS derived with legacy SPL token program.
//      User's personal ATA uses the detected program (for Token-2022 mints).

use anyhow::{anyhow, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program, sysvar,
};
use std::str::FromStr;

pub const PUMP_PROGRAM_ID:        &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const EVENT_AUTHORITY:        &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7jxXpXhH";
pub const FEE_RECIPIENT_FALLBACK: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
pub const LEGACY_TOKEN_PROGRAM:   &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM:     &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

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
            Self::Legacy    => Pubkey::from_str(LEGACY_TOKEN_PROGRAM).unwrap(),
            Self::Token2022 => Pubkey::from_str(TOKEN_2022_PROGRAM).unwrap(),
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
//  Runtime detection — reads mint account owner from chain
// ─────────────────────────────────────────────────────────────────────────────

pub async fn detect_token_program(rpc: &RpcClient, mint: &Pubkey) -> TokenProgram {
    match rpc.get_account(mint).await {
        Ok(account) => {
            let owner = account.owner.to_string();
            if owner == TOKEN_2022_PROGRAM {
                tracing::debug!("{}… → Token-2022", &mint.to_string()[..8]);
                TokenProgram::Token2022
            } else {
                tracing::debug!("{}… → legacy SPL (owner: {})", &mint.to_string()[..8], &owner[..8]);
                TokenProgram::Legacy
            }
        }
        Err(e) => {
            tracing::warn!("detect_token_program {}… failed: {} — using legacy", &mint.to_string()[..8], e);
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
    let bytes: [u8; 32] = data[41..73].try_into()
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

/// Bonding curve ATA — ALWAYS uses legacy SPL token program.
/// Pump.fun hardcodes this regardless of the mint's token program.
pub fn associated_bonding_curve(mint: &Pubkey) -> Pubkey {
    let legacy_pid = Pubkey::from_str(LEGACY_TOKEN_PROGRAM).unwrap();
    crate::utils::get_ata_with_program(&bonding_curve_pda(mint), mint, &legacy_pid)
}

/// User's personal ATA — uses the detected token program (may be Token-2022).
pub fn user_ata(payer: &Pubkey, mint: &Pubkey, tok: TokenProgram) -> Pubkey {
    crate::utils::get_ata_with_program(payer, mint, &tok.pubkey())
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
    let pid = program_id();
    // User ATA: uses detected program (Token-2022 or legacy)
    let u_ata = user_ata(payer, mint, tok);
    // Bonding curve ATA: ALWAYS legacy (Pump.fun requirement)
    let bc_ata = associated_bonding_curve(mint);

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
            AccountMeta::new(bc_ata,                                                   false),
            AccountMeta::new(u_ata,                                                    false),
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
    let pid   = program_id();
    let u_ata = user_ata(payer, mint, tok);
    let bc_ata = associated_bonding_curve(mint);

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
            AccountMeta::new(bc_ata,                                                   false),
            AccountMeta::new(u_ata,                                                    false),
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
