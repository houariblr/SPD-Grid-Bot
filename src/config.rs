// ═══════════════════════════════════════════════════════════
//  config.rs — SPD Software Lab  v3.0 PRODUCTION
//  Loads from config.toml + .env for secrets
// ═══════════════════════════════════════════════════════════

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    // ── Trading pair ─────────────────────────────────────────
    pub symbol:                    String,
    pub total_capital:             f64,
    pub grid_levels:               usize,

    // ── Risk ─────────────────────────────────────────────────
    pub max_loss_pct:              f64,
    pub max_drawdown_pct:          f64,
    pub profit_target_pct:         f64,
    pub max_single_trade_pct:      f64,
    pub min_notional_usdt:         f64,

    // ── Execution ────────────────────────────────────────────
    pub slippage_pct:              f64,

    // ── Grid rebuild ─────────────────────────────────────────
    pub grid_rebuild_ticks:        u64,
    #[serde(default = "d_atr_rebuild")]
    pub atr_rebuild_threshold_pct: f64,
    #[serde(default = "d_atr_cap")]
    pub atr_outlier_cap_pct:       f64,

    // ── Network ──────────────────────────────────────────────
    pub rate_limit_per_sec:        u64,
    pub max_retries:               u32,
    pub retry_base_sec:            u64,
    pub latency_buf_size:          usize,
    pub max_trade_history:         usize,

    // ── Logging ──────────────────────────────────────────────
    #[serde(default = "d_heartbeat")]
    pub heartbeat_secs:            u64,
    #[serde(default = "d_status")]
    pub status_secs:               u64,

    // ── Copy-trade server ────────────────────────────────────
    #[serde(default = "d_false")]
    pub copy_trade_enabled:        bool,
    #[serde(default = "d_ct_port")]
    pub copy_trade_port:           u16,
    /// Fee % of follower profit sent back to master
    #[serde(default = "d_fee_pct")]
    pub fee_pct:                   f64,
    /// Master wallet for fee payments (on-chain or Binance UID)
    #[serde(default = "d_empty")]
    pub master_id:                 String,

    // ── Mode ─────────────────────────────────────────────────
    /// "live" | "paper" | "backtest"
    #[serde(default = "d_paper")]
    pub mode:                      String,
}

// ── Defaults ─────────────────────────────────────────────────
fn d_atr_rebuild() -> f64  { 15.0   }
fn d_atr_cap()     -> f64  { 5.0    }
fn d_heartbeat()   -> u64  { 30     }
fn d_status()      -> u64  { 30     }
fn d_false()       -> bool { false  }
fn d_ct_port()     -> u16  { 8080   }
fn d_fee_pct()     -> f64  { 10.0   }
fn d_empty()       -> String { String::new() }
fn d_paper()       -> String { "paper".into() }

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            symbol:                    "BNBUSDT".into(),
            total_capital:             20.0,
            grid_levels:               8,
            max_loss_pct:              5.0,
            max_drawdown_pct:          8.0,
            profit_target_pct:         1.0,
            max_single_trade_pct:      0.09,
            min_notional_usdt:         5.0,
            slippage_pct:              0.0005,
            grid_rebuild_ticks:        240,
            atr_rebuild_threshold_pct: 15.0,
            atr_outlier_cap_pct:       5.0,
            rate_limit_per_sec:        10,
            max_retries:               5,
            retry_base_sec:            2,
            latency_buf_size:          100,
            max_trade_history:         10_000,
            heartbeat_secs:            30,
            status_secs:               60,
            copy_trade_enabled:        false,
            copy_trade_port:           8080,
            fee_pct:                   10.0,
            master_id:                 String::new(),
            mode:                      "paper".into(),
        }
    }
}

impl BotConfig {
    pub async fn load(path: &str) -> Self {
        match tokio::fs::read_to_string(path).await {
            Err(e) => {
                eprintln!("⚠️  config.toml not found ({}) — using defaults", e);
                Self::default()
            }
            Ok(content) => match toml::from_str::<Self>(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("❌ config.toml parse error: {} — using defaults", e);
                    Self::default()
                }
            }
        }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.total_capital <= 0.0 {
            errs.push("total_capital must be > 0".into());
        }
        if self.grid_levels < 2 || self.grid_levels % 2 != 0 {
            errs.push("grid_levels must be even and ≥ 2".into());
        }
        if !(0.0 < self.max_loss_pct && self.max_loss_pct < 100.0) {
            errs.push("max_loss_pct must be 0..100".into());
        }
        if !(0.0 < self.max_single_trade_pct && self.max_single_trade_pct <= 1.0) {
            errs.push("max_single_trade_pct must be 0..1".into());
        }
        if self.slippage_pct < 0.0 || self.slippage_pct > 0.01 {
            errs.push("slippage_pct must be 0..0.01".into());
        }
        if self.min_notional_usdt < 1.0 {
            errs.push("min_notional_usdt must be ≥ 1".into());
        }
        if self.mode == "live" {
            let key = std::env::var("BINANCE_API_KEY").unwrap_or_default();
            let sec = std::env::var("BINANCE_API_SECRET").unwrap_or_default();
            if key.is_empty() || sec.is_empty() {
                errs.push("LIVE mode requires BINANCE_API_KEY + BINANCE_API_SECRET in .env".into());
            }
        }
        errs
    }

    pub fn symbol_lower(&self) -> String { self.symbol.to_lowercase() }
    pub fn is_live(&self)    -> bool { self.mode == "live"     }
    pub fn is_paper(&self)   -> bool { self.mode == "paper"    }
    pub fn is_backtest(&self)-> bool { self.mode == "backtest" }

    pub fn api_key(&self)    -> String { std::env::var("BINANCE_API_KEY").unwrap_or_default() }
    pub fn api_secret(&self) -> String { std::env::var("BINANCE_API_SECRET").unwrap_or_default() }
}
