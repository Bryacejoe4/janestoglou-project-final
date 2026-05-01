// src/dex/raydium.rs
// Phase 2 complete:
//   • RaydiumPoolInfo struct with all accounts needed for a swap
//   • fetch_pool_for_mint() — gets pool via Raydium API then decodes AMM + serum accounts from RPC
//   • build_swap_instruction — unchanged from Phase 1

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::str::FromStr;

pub const AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const SERUM_PROGRAM:  &str = "srmqPvymJeFKQ4zdt99No696YSeB68Y57zbyC5vM7F";
pub const TOKEN_PROGRAM:  &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const SOL_MINT:       &str = "So11111111111111111111111111111111111111112";

const RAYDIUM_PAIRS_API: &str = "https://api.raydium.io/v2/main/pairs";

pub fn program_id() -> Pubkey { AMM_V4_PROGRAM.parse().expect("const") }

// ─────────────────────────────────────────────────────────────────────────────
//  Pool info — all accounts needed to build a swap instruction
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RaydiumPoolInfo {
    pub amm_id:                  Pubkey,
    pub amm_authority:           Pubkey,
    pub amm_open_orders:         Pubkey,
    pub amm_target_orders:       Pubkey,
    pub pool_coin_token_account: Pubkey,
    pub pool_pc_token_account:   Pubkey,
    pub serum_market:            Pubkey,
    pub serum_bids:              Pubkey,
    pub serum_asks:              Pubkey,
    pub serum_event_queue:       Pubkey,
    pub serum_coin_vault:        Pubkey,
    pub serum_pc_vault:          Pubkey,
    pub serum_vault_signer:      Pubkey,
    /// true if base = SOL, false if quote = SOL (affects swap direction)
    pub base_is_sol:             bool,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Raydium pairs API response
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RaydiumPair {
    amm_id:     Option<String>,
    base_mint:  Option<String>,
    quote_mint: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
//  Pool fetcher — Raydium API → RPC decode
// ─────────────────────────────────────────────────────────────────────────────

/// Fetch all accounts needed for a Raydium V4 swap for the given token mint.
/// Strategy:
///   1. Call Raydium pairs API to find the AMM ID
///   2. Fetch AMM account from RPC and decode vault / order addresses
///   3. Fetch serum market account from RPC and decode bids/asks/vaults
pub async fn fetch_pool_for_mint(
    rpc:  &RpcClient,
    http: &Client,
    mint: &str,
) -> Result<RaydiumPoolInfo> {
    // ── Step 1: Find AMM ID via Raydium pairs API ─────────────────────────
    tracing::info!("Fetching Raydium pool for {}…", &mint[..8.min(mint.len())]);

    let pairs: Vec<RaydiumPair> = http
        .get(RAYDIUM_PAIRS_API)
        .timeout(std::time::Duration::from_secs(10))
        .send().await
        .map_err(|e| anyhow!("Raydium API request: {}", e))?
        .json().await
        .map_err(|e| anyhow!("Raydium API parse: {}", e))?;

    let sol = SOL_MINT;
    let pair = pairs.iter().find(|p| {
        let bm = p.base_mint.as_deref().unwrap_or("");
        let qm = p.quote_mint.as_deref().unwrap_or("");
        p.amm_id.is_some() && (
            (bm == mint && qm == sol) ||
            (qm == mint && bm == sol)
        )
    }).ok_or_else(|| anyhow!("No Raydium pool found for mint {}", mint))?;

    let amm_id_str = pair.amm_id.as_ref().unwrap();
    let base_is_sol = pair.quote_mint.as_deref() == Some(mint); // quote=token means base=SOL
    let amm_id = Pubkey::from_str(amm_id_str)
        .map_err(|_| anyhow!("Invalid AMM ID: {}", amm_id_str))?;

    tracing::info!("Found Raydium AMM: {}", amm_id);

    // ── Step 2: Decode AMM account ────────────────────────────────────────
    let amm_data = rpc.get_account_data(&amm_id).await
        .map_err(|e| anyhow!("Fetch AMM account: {}", e))?;

    let amm = decode_amm_account(&amm_data)?;

    // AMM authority is a fixed PDA for Raydium V4
    let amm_authority = amm_authority_pda();

    // ── Step 3: Decode serum market account ───────────────────────────────
    let serum_data = rpc.get_account_data(&amm.serum_market).await
        .map_err(|e| anyhow!("Fetch serum market: {}", e))?;

    let serm = decode_serum_market(&serum_data, &amm.serum_market)?;

    Ok(RaydiumPoolInfo {
        amm_id,
        amm_authority,
        amm_open_orders:         amm.amm_open_orders,
        amm_target_orders:       amm.amm_target_orders,
        pool_coin_token_account: amm.pool_coin_token_account,
        pool_pc_token_account:   amm.pool_pc_token_account,
        serum_market:            amm.serum_market,
        serum_bids:              serm.bids,
        serum_asks:              serm.asks,
        serum_event_queue:       serm.event_queue,
        serum_coin_vault:        serm.base_vault,
        serum_pc_vault:          serm.quote_vault,
        serum_vault_signer:      serm.vault_signer,
        base_is_sol,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
//  AMM account decoder
//
//  Raydium AMM V4 AmmInfo layout (Borsh, no discriminator):
//    28 × u64  = 224 bytes
//    4  × u128 = 64 bytes → total 288
//    1  × u64  = 8  bytes → 296
//    2  × u128 = 32 bytes → 328
//    1  × u64  = 8  bytes → 336   ← first Pubkey starts here
//
//  Pubkeys in order (each 32 bytes):
//    pool_coin_token_account  336
//    pool_pc_token_account    368
//    coin_mint                400
//    pc_mint                  432
//    lp_mint                  464
//    amm_open_orders          496
//    serum_market             528
//    serum_program_id         560
//    amm_target_orders        592
// ─────────────────────────────────────────────────────────────────────────────

struct AmmAccounts {
    pool_coin_token_account: Pubkey,
    pool_pc_token_account:   Pubkey,
    amm_open_orders:         Pubkey,
    serum_market:            Pubkey,
    amm_target_orders:       Pubkey,
}

fn decode_amm_account(data: &[u8]) -> Result<AmmAccounts> {
    if data.len() < 624 {
        return Err(anyhow!("AMM account too short: {} bytes", data.len()));
    }
    let read_pk = |offset: usize| -> Result<Pubkey> {
        let slice: [u8; 32] = data[offset..offset + 32]
            .try_into()
            .map_err(|_| anyhow!("pk slice error at offset {}", offset))?;
        Ok(Pubkey::new_from_array(slice))
    };

    Ok(AmmAccounts {
        pool_coin_token_account: read_pk(336)?,
        pool_pc_token_account:   read_pk(368)?,
        amm_open_orders:         read_pk(496)?,
        serum_market:            read_pk(528)?,
        amm_target_orders:       read_pk(592)?,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
//  Serum market decoder
//
//  MarketState V2 layout (5-byte padding header first):
//    [5 padding]
//    accountFlags u64     (5)
//    ownAddress   Pubkey  (13)
//    vaultSignerNonce u64 (45)
//    baseMint     Pubkey  (53)
//    quoteMint    Pubkey  (85)
//    baseVault    Pubkey  (117)  ← serum_coin_vault
//    baseDepositsTotal u64 (149)
//    baseFeesAccrued u64  (157)
//    quoteVault   Pubkey  (165)  ← serum_pc_vault
//    quoteDepositsTotal u64 (197)
//    quoteFeesAccrued u64  (205)
//    quoteDustThreshold u64 (213)
//    requestQueue Pubkey  (221)
//    eventQueue   Pubkey  (253)  ← serum_event_queue
//    bids         Pubkey  (285)  ← serum_bids
//    asks         Pubkey  (317)  ← serum_asks
// ─────────────────────────────────────────────────────────────────────────────

struct SerumMarketAccounts {
    base_vault:   Pubkey,
    quote_vault:  Pubkey,
    event_queue:  Pubkey,
    bids:         Pubkey,
    asks:         Pubkey,
    vault_signer: Pubkey,
}

fn decode_serum_market(data: &[u8], market_address: &Pubkey) -> Result<SerumMarketAccounts> {
    if data.len() < 349 {
        return Err(anyhow!("Serum market account too short: {} bytes", data.len()));
    }

    let read_pk = |offset: usize| -> Result<Pubkey> {
        let slice: [u8; 32] = data[offset..offset + 32]
            .try_into()
            .map_err(|_| anyhow!("serum pk slice at {}", offset))?;
        Ok(Pubkey::new_from_array(slice))
    };

    let vault_signer_nonce = u64::from_le_bytes(
        data[45..53].try_into().map_err(|_| anyhow!("nonce slice"))?
    );

    let serum_pid = Pubkey::from_str(SERUM_PROGRAM).unwrap();
    let vault_signer = Pubkey::create_program_address(
        &[market_address.as_ref(), &vault_signer_nonce.to_le_bytes()],
        &serum_pid,
    ).map_err(|e| anyhow!("vault_signer PDA: {}", e))?;

    Ok(SerumMarketAccounts {
        base_vault:  read_pk(117)?,
        quote_vault: read_pk(165)?,
        event_queue: read_pk(253)?,
        bids:        read_pk(285)?,
        asks:        read_pk(317)?,
        vault_signer,
    })
}

// AMM authority PDA — fixed for Raydium V4
fn amm_authority_pda() -> Pubkey {
    let pid = program_id();
    Pubkey::find_program_address(&[b"amm authority"], &pid).0
}

// ─────────────────────────────────────────────────────────────────────────────
//  Swap instruction builder — unchanged from Phase 1
// ─────────────────────────────────────────────────────────────────────────────

pub fn build_swap_instruction(
    pool:           &RaydiumPoolInfo,
    user_owner:     &Pubkey,
    user_source:    &Pubkey,
    user_dest:      &Pubkey,
    amount_in:      u64,
    min_amount_out: u64,
) -> Instruction {
    let pid       = program_id();
    let token_pid = TOKEN_PROGRAM.parse::<Pubkey>().expect("const");
    let serum_pid = SERUM_PROGRAM.parse::<Pubkey>().expect("const");

    let mut data = Vec::with_capacity(17);
    data.push(9u8);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    Instruction {
        program_id: pid,
        accounts: vec![
            AccountMeta::new_readonly(token_pid,                                  false),
            AccountMeta::new(pool.amm_id,                                         false),
            AccountMeta::new_readonly(pool.amm_authority,                         false),
            AccountMeta::new(pool.amm_open_orders,                                false),
            AccountMeta::new(pool.amm_target_orders,                              false),
            AccountMeta::new(pool.pool_coin_token_account,                        false),
            AccountMeta::new(pool.pool_pc_token_account,                          false),
            AccountMeta::new_readonly(serum_pid,                                  false),
            AccountMeta::new(pool.serum_market,                                   false),
            AccountMeta::new(pool.serum_bids,                                     false),
            AccountMeta::new(pool.serum_asks,                                     false),
            AccountMeta::new(pool.serum_event_queue,                              false),
            AccountMeta::new(pool.serum_coin_vault,                               false),
            AccountMeta::new(pool.serum_pc_vault,                                 false),
            AccountMeta::new_readonly(pool.serum_vault_signer,                    false),
            AccountMeta::new(*user_source,                                         false),
            AccountMeta::new(*user_dest,                                           false),
            AccountMeta::new_readonly(*user_owner,                                 true),
        ],
        data,
    }
}

fn copy_pubkey(pk: &Pubkey) -> Pubkey { Pubkey::new_from_array(pk.to_bytes()) }
