// src/telegram.rs
// Sends real-time trade alerts to a Telegram bot.

// If TELEGRAM_BOT_TOKEN is not set, all calls are silent no-ops.

use reqwest::Client;

const TELEGRAM_API: &str = "https://api.telegram.org/bot";

// ─────────────────────────────────────────────────────────────────────────────
//  TelegramBot
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TelegramBot {
    http:     Client,
    token:    Option<String>,
    chat_id:  Option<String>,
    enabled:  bool,
}

impl TelegramBot {
    /// Load credentials from .env. Silent no-op if not configured.
    pub fn from_env() -> Self {
        let token   = std::env::var("TELEGRAM_BOT_TOKEN").ok();
        let chat_id = std::env::var("TELEGRAM_CHAT_ID").ok();
        let enabled = token.is_some() && chat_id.is_some();

        if enabled {
            tracing::info!("✅ Telegram alerts enabled");
        } else {
            tracing::info!("Telegram alerts disabled (set TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID in .env to enable)");
        }

        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            token,
            chat_id,
            enabled,
        }
    }

    // ── Public alert methods ──────────────────────────────────────────────

    /// 🟢 BUY alert
    pub async fn alert_buy(
        &self,
        mint:      &str,
        sol_spent: f64,
        tokens:    u64,
        sol_usd:   f64,
        sig:       &str,
    ) {
        let usd = sol_spent * sol_usd;
        let usd_str = if usd > 0.0 { format!(" (${:.2})", usd) } else { String::new() };
        let msg = format!(
            "🟢 *BUY EXECUTED*\n\
             Token: `{mint}`\n\
             Spent: `{sol:.4} SOL{usd_str}`\n\
             Tokens: `{tokens}`\n\
             [View on Solscan](https://solscan.io/tx/{sig})",
            mint    = shorten(mint),
            sol     = sol_spent,
            usd_str = usd_str,
            tokens  = tokens,
            sig     = sig,
        );
        self.send(&msg).await;
    }

    /// 🔴 SELL alert
    pub async fn alert_sell(
        &self,
        mint:    &str,
        pnl_sol: f64,
        sol_usd: f64,
        reason:  &str,
        sig:     &str,
    ) {
        let emoji     = if pnl_sol >= 0.0 { "💰" } else { "🔴" };
        let sign      = if pnl_sol >= 0.0 { "+" } else { "" };
        let pnl_usd   = pnl_sol * sol_usd;
        let usd_str   = if sol_usd > 0.0 { format!(" (${:+.2})", pnl_usd) } else { String::new() };
        let msg = format!(
            "{emoji} *SELL EXECUTED* — {reason}\n\
             Token: `{mint}`\n\
             PnL:   `{sign}{pnl_sol:.4} SOL{usd_str}`\n\
             [View on Solscan](https://solscan.io/tx/{sig})",
            emoji   = emoji,
            reason  = reason,
            mint    = shorten(mint),
            sign    = sign,
            pnl_sol = pnl_sol,
            usd_str = usd_str,
            sig     = sig,
        );
        self.send(&msg).await;
    }

    /// 🎓 Graduation alert
    pub async fn alert_graduation(&self, mint: &str) {
        let msg = format!(
            "🎓 *TOKEN GRADUATED*\n\
             `{}` has completed its Pump.fun bonding curve.\n\
             Future sells will route through *Raydium*.",
            shorten(mint)
        );
        self.send(&msg).await;
    }

    /// 🛑 Stop loss alert
    pub async fn alert_stop_loss(&self, mint: &str, loss_pct: f64) {
        let msg = format!(
            "🛑 *STOP LOSS TRIGGERED*\n\
             Token: `{}`\n\
             Loss:  `{:.1}%`",
            shorten(mint), loss_pct
        );
        self.send(&msg).await;
    }

    /// ⚠️ Daily limit alert
    pub async fn alert_daily_limit_paused(&self, loss_pct: f64, _sol_usd: f64) {
        let msg = format!(
            "⚠️ *BOT PAUSED — DAILY LOSS LIMIT HIT*\n\
             Total loss today: `{:.1}%`\n\
             Resume with: `botx run` tomorrow or adjust `daily_loss_limit_pct` in config.",
            loss_pct
        );
        self.send(&msg).await;
    }

    /// 🚀 Bot started alert
    pub async fn alert_bot_started(&self, wallet: &str, balance_sol: f64, sol_usd: f64) {
        let balance_usd = balance_sol * sol_usd;
        let usd_str = if sol_usd > 0.0 { format!(" (${:.2})", balance_usd) } else { String::new() };
        let msg = format!(
            "🚀 *BOT STARTED* — solana-hft-botx v0.3.0\n\
             Wallet:  `{}`\n\
             Balance: `{:.4} SOL{}`",
            shorten(wallet), balance_sol, usd_str
        );
        self.send(&msg).await;
    }

    /// ✅ Copy trade signal alert
    pub async fn alert_copy_trade(&self, action: &str, wallet: &str, mint: &str) {
        let emoji = if action == "BUY" { "📋" } else { "📤" };
        let msg   = format!(
            "{emoji} *COPY TRADE {action}*\n\
             Wallet: `{}`\n\
             Token:  `{}`",
            shorten(wallet), shorten(mint),
            emoji  = emoji,
            action = action,
        );
        self.send(&msg).await;
    }

    // ── Core sender ───────────────────────────────────────────────────────

    /// Send a Markdown-formatted message. Silent no-op if not configured.
    async fn send(&self, text: &str) {
        if !self.enabled { return; }
        let Some(token)   = &self.token   else { return };
        let Some(chat_id) = &self.chat_id else { return };

        let url  = format!("{}{}/sendMessage", TELEGRAM_API, token);
        let body = serde_json::json!({
            "chat_id":    chat_id,
            "text":       text,
            "parse_mode": "Markdown",
            "disable_web_page_preview": true,
        });

        match self.http.post(&url).json(&body).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    tracing::warn!("Telegram send failed: {}", resp.status());
                }
            }
            Err(e) => tracing::warn!("Telegram request error: {}", e),
        }
    }

    pub fn is_enabled(&self) -> bool { self.enabled }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Shorten a pubkey/address for display: first 4 + last 4 chars.
fn shorten(s: &str) -> String {
    if s.len() <= 12 { return s.to_string(); }
    format!("{}…{}", &s[..6], &s[s.len()-4..])
}
