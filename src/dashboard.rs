// src/dashboard.rs
// New: displays USD values alongside SOL values using live SOL/USD price.

use crate::logic::Position;
use dashmap::DashMap;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub mint:       String,
    pub action:     String,
    pub sol_amount: f64,
    pub pnl_sol:    Option<f64>,
    pub pnl_usd:    Option<f64>,   // Phase 2: USD PnL
    pub sig:        String,
    pub timestamp:  chrono::DateTime<chrono::Utc>,
}

#[derive(Default)]
pub struct DashboardState {
    pub positions:     DashMap<String, Position>,
    pub trade_log:     RwLock<Vec<TradeRecord>>,
    pub sol_balance:   RwLock<f64>,
    pub start_balance: RwLock<f64>,
    pub trade_count:   RwLock<u32>,
    pub error_count:   RwLock<u32>,
    pub sol_usd:       RwLock<f64>,   // Phase 2: live SOL/USD price
}

impl DashboardState {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    pub fn record_trade(&self, record: TradeRecord) {
        let mut log = self.trade_log.write();
        log.push(record);
        if log.len() > 50 { log.remove(0); }
        *self.trade_count.write() += 1;
    }

    pub fn record_error(&self)          { *self.error_count.write() += 1; }
    pub fn update_balance(&self, sol: f64) { *self.sol_balance.write() = sol; }
    pub fn update_sol_usd(&self, usd: f64) { *self.sol_usd.write() = usd; }

    pub fn total_pnl_sol(&self) -> f64 {
        self.trade_log.read().iter().filter_map(|r| r.pnl_sol).sum()
    }

    pub fn open_position_count(&self) -> usize {
        self.positions.iter().filter(|p| !p.is_closed).count()
    }

    pub fn sol_to_usd(&self, sol: f64) -> f64 {
        let rate = *self.sol_usd.read();
        if rate > 0.0 { sol * rate } else { 0.0 }
    }
}

pub async fn run_dashboard(state: Arc<DashboardState>, interval_secs: u64) {
    let interval = std::time::Duration::from_secs(interval_secs);
    loop {
        tokio::time::sleep(interval).await;
        print_dashboard(&state);
    }
}

fn print_dashboard(state: &DashboardState) {
    let now         = chrono::Utc::now().format("%H:%M:%S UTC");
    let balance_sol = *state.sol_balance.read();
    let start_bal   = *state.start_balance.read();
    let total_pnl   = state.total_pnl_sol();
    let trade_count = *state.trade_count.read();
    let error_count = *state.error_count.read();
    let open_pos    = state.open_position_count();
    let sol_usd     = *state.sol_usd.read();
    let pnl_pct     = if start_bal > 0.0 { (total_pnl / start_bal) * 100.0 } else { 0.0 };
    let pnl_sign    = if total_pnl >= 0.0 { "+" } else { "" };

    let balance_usd = state.sol_to_usd(balance_sol);
    let pnl_usd     = state.sol_to_usd(total_pnl);

    let div = "═".repeat(66);
    let sub = "─".repeat(66);
    println!("\n{}", div);
    println!("  solana-hft-botx  ·  {}  ·  v0.3.0", now);
    println!("{}", div);

    // SOL/USD rate line
    if sol_usd > 0.0 {
        println!("  SOL/USD:     ${:.2}", sol_usd);
    }

    // Balance line — shows both SOL and USD
    if sol_usd > 0.0 {
        println!(
            "  Balance:     {:.4} SOL  (${:.2})",
            balance_sol, balance_usd
        );
    } else {
        println!("  Balance:     {:.4} SOL", balance_sol);
    }

    // PnL line
    if sol_usd > 0.0 {
        println!(
            "  Session PnL: {}{:.4} SOL  ({}{:.1}%)  ({}{:.2} USD)",
            pnl_sign, total_pnl, pnl_sign, pnl_pct, pnl_sign, pnl_usd
        );
    } else {
        println!(
            "  Session PnL: {}{:.4} SOL  ({}{:.1}%)",
            pnl_sign, total_pnl, pnl_sign, pnl_pct
        );
    }

    println!(
        "  Open pos:    {}   |   Trades: {}   |   Errors: {}",
        open_pos, trade_count, error_count
    );
    println!("{}", sub);

    // Open positions
    if open_pos > 0 {
        println!("  {:12}  {:>10}  {:>10}  {:>8}", "MINT", "ENTRY SOL", "PEAK SOL", "PNL%");
        for entry in state.positions.iter() {
            let pos  = entry.value();
            if pos.is_closed { continue; }
            let cost = pos.avg_cost();
            let peak = pos.peak_price_sol;
            let pct  = if cost > 0.0 { ((peak - cost) / cost) * 100.0 } else { 0.0 };
            let sign = if pct >= 0.0 { "+" } else { "" };
            println!(
                "  {:12}  {:>10.8}  {:>10.8}  {:>6}{:.1}%",
                &pos.mint[..8.min(pos.mint.len())],
                cost, peak, sign, pct
            );
        }
        println!("{}", sub);
    }

    // Last 5 trades
    let log    = state.trade_log.read();
    let recent: Vec<_> = log.iter().rev().take(5).collect();
    if !recent.is_empty() {
        println!("  RECENT TRADES");
        for t in &recent {
            let pnl = match (t.pnl_sol, t.pnl_usd) {
                (Some(s), Some(u)) if sol_usd > 0.0 =>
                    format!("{:+.4} SOL  ({:+.2} USD)", s, u),
                (Some(s), _) =>
                    format!("{:+.4} SOL", s),
                _ => "─".to_string(),
            };
            println!(
                "  {} {:>4}  {}…  {:.4} SOL  {}",
                t.timestamp.format("%H:%M:%S"),
                t.action,
                &t.mint[..8.min(t.mint.len())],
                t.sol_amount,
                pnl,
            );
        }
    }
    println!("{}", div);
}
