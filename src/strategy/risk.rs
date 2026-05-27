// src/strategy/risk.rs
// Risk management: position sizing, daily-loss circuit-breaker, stop enforcement.

use anyhow::{anyhow, Result};
use crate::{config::RiskConfig, logic::DailyRiskState};

// ─────────────────────────────────────────────────────────────────────────────
//  RiskManager
// ─────────────────────────────────────────────────────────────────────────────

pub struct RiskManager {
    pub cfg:   RiskConfig,
    pub state: DailyRiskState,
    paused:    bool,
}

impl RiskManager {
    pub fn new(cfg: RiskConfig, starting_balance_lamports: u64) -> Self {
        Self {
            cfg,
            state: DailyRiskState {
                starting_balance_lamports,
                ..Default::default()
            },
            paused: false,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Pre-trade checks
    // ─────────────────────────────────────────────────────────────────────────

    /// Returns the lamport amount we are allowed to spend on the next trade.
    /// Returns Err if trading is paused for any reason.
    pub fn allowed_trade_size(
        &self,
        current_balance_lamports: u64,
        open_position_count: usize,
        last_buy_timestamp:  std::time::Instant,
    ) -> Result<u64> {
        if self.paused {
            return Err(anyhow!("Bot paused: daily loss limit was breached"));
        }
        if self.state.limit_breached(self.cfg.daily_loss_limit_pct) {
            return Err(anyhow!("Daily loss limit reached"));
        }

        // SOL floor — never trade below 0.05 SOL reserve
        let floor_lamports = 50_000_000u64;
        if current_balance_lamports <= floor_lamports {
            return Err(anyhow!("Balance below SOL floor reserve (0.05 SOL)"));
        }

        // Concurrent position cap
        if open_position_count >= self.cfg.max_open_positions {
            return Err(anyhow!("Max concurrent positions reached ({})", self.cfg.max_open_positions));
        }

        // Buy cooldown
        if last_buy_timestamp.elapsed() < std::time::Duration::from_secs(self.cfg.buy_cooldown_secs) {
            return Err(anyhow!("Buy cooldown active, wait {}s between buys", self.cfg.buy_cooldown_secs));
        }

        let tradeable_balance = current_balance_lamports.saturating_sub(floor_lamports);
        let position_cap = (tradeable_balance as f64 * self.cfg.max_position_pct) as u64;
        let allowed = position_cap.min(self.cfg.max_sol_per_trade);

        if allowed == 0 {
            return Err(anyhow!("Calculated trade size is 0"));
        }

        Ok(allowed)
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Post-trade accounting
    // ─────────────────────────────────────────────────────────────────────────
    /*
    pub fn record_trade(&mut self, pnl_lamports: i64) {
        self.state.realised_pnl_lamports += pnl_lamports;
        self.state.trade_count           += 1;

        if self.state.limit_breached(self.cfg.daily_loss_limit_pct) {
            tracing::warn!(
                "⚠️  Daily loss limit hit ({:.1}%)! Bot is now PAUSED.",
                self.state.loss_fraction() * 100.0
            );
            self.paused = true;
        }
    }
    */
    pub fn record_buy(&mut self, lamports_spent: u64) {
        self.state.unrealized_spend_lamports =
            self.state.unrealized_spend_lamports.saturating_add(lamports_spent);
    }

    pub fn record_sell(&mut self, pnl_lamports: i64, lamports_spent: u64) {
        self.state.realised_pnl_lamports += pnl_lamports;
        self.state.trade_count += 1;
        self.state.unrealized_spend_lamports =
            self.state.unrealized_spend_lamports.saturating_sub(lamports_spent);

        if self.state.limit_breached(self.cfg.daily_loss_limit_pct) {
            tracing::warn!("⚠️ Daily loss limit hit! Bot is now PAUSED.");
            self.paused = true;
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    //  Manual controls
    // ─────────────────────────────────────────────────────────────────────────

    pub fn pause(&mut self)  { self.paused = true; }
    pub fn resume(&mut self) { self.paused = false; }
    pub fn is_paused(&self) -> bool { self.paused }

    /// Reset daily state (call at UTC midnight or bot restart).
    pub fn reset_daily(&mut self, new_balance: u64) {
        self.state = DailyRiskState {
            starting_balance_lamports: new_balance,
            ..Default::default()
        };
        self.paused = false;
        tracing::info!("Daily risk state reset. Starting balance: {:.4} SOL",
            crate::utils::lamports_to_sol(new_balance));
    }

    pub fn summary(&self) -> String {
        format!(
            "Trades: {} | PnL: {:+.4} SOL | Loss%: {:.1}% | Paused: {}",
            self.state.trade_count,
            self.state.realised_pnl_lamports as f64 / 1e9,
            self.state.loss_fraction() * 100.0,
            self.paused,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
//  Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cfg() -> RiskConfig {
        RiskConfig {
            max_position_pct:     0.15,
            daily_loss_limit_pct: 0.10,
            max_sol_per_trade:    500_000_000,
        }
    }

    #[test]
    fn caps_at_max_position() {
        let rm = RiskManager::new(default_cfg(), 10_000_000_000);
        let size = rm.allowed_trade_size(10_000_000_000).unwrap();
        assert_eq!(size, 500_000_000); // 15% of 10 SOL = 1.5 SOL, capped at 0.5
    }

    #[test]
    fn pauses_on_daily_limit() {
        let mut rm = RiskManager::new(default_cfg(), 1_000_000_000);
        rm.record_trade(-110_000_000); // -0.11 SOL loss on 1 SOL = 11 %
        assert!(rm.is_paused());
        assert!(rm.allowed_trade_size(900_000_000).is_err());
    }
}
