// ═══════════════════════════════════════════════════════════
//  engine.rs — SPD Software Lab  v3.0 PRODUCTION
//  FIXES:
//  [FIX-2] TradingStatus.is_active() — no PartialEq on Instant
//  [FIX-6] drain(0..1000) → pop_front() O(1)
//  [FIX-7] BacktestStatus uses tick counter not Instant
//  [FIX-8] Unified structs — no duplicates with main.rs
//  [V1]    ATR-relative grid spacing
//  [T1]    TrendSignal position/spacing multipliers
//  [W1]    Variable slippage via WickProfile
//  [O1]    Auto-optimizer with Sharpe + Calmar
// ═══════════════════════════════════════════════════════════

use crate::models::{
    GridLevel, MarketState, OptResult, Regime, Side,
    TradeRecord, TradingStatus, Trend, TrendSignal, regime_bucket,
};
use crate::config::BotConfig;
use std::collections::{HashSet, VecDeque};
use std::time::Instant;
use chrono::Local;
use tracing::{info, warn};
use uuid::Uuid;

// ══════════════════════════════════════════════════════════
//  [FIX-7] Backtest-specific status (uses tick counter)
// ══════════════════════════════════════════════════════════
pub enum BacktestStatus {
    Active,
    PausedUntilTick(u64),   // ← tick number, not Instant
    Stopped(String),
}

impl BacktestStatus {
    pub fn is_active(&self)  -> bool { matches!(self, Self::Active) }
    pub fn is_stopped(&self) -> bool { matches!(self, Self::Stopped(_)) }
}

// ══════════════════════════════════════════════════════════
//  RISK MANAGER  (shared by live + backtest)
// ══════════════════════════════════════════════════════════
pub struct RiskManager {
    pub initial_capital:   f64,
    pub peak_capital:      f64,
    pub max_loss_pct:      f64,
    pub max_drawdown_pct:  f64,
    pub profit_target_pct: f64,
    pub max_single_trade:  f64,
    pub safety_dd_pct:     f64,
    pub daily_returns:     Vec<f64>,
    prev_snapshot:         f64,
}

impl RiskManager {
    pub fn new(cfg: &BotConfig) -> Self {
        Self {
            initial_capital:   cfg.total_capital,
            peak_capital:      cfg.total_capital,
            max_loss_pct:      cfg.max_loss_pct,
            max_drawdown_pct:  cfg.max_drawdown_pct,
            profit_target_pct: cfg.profit_target_pct,
            max_single_trade:  cfg.max_single_trade_pct,
            safety_dd_pct:     cfg.max_drawdown_pct * 1.5,
            daily_returns:     Vec::new(),
            prev_snapshot:     cfg.total_capital,
        }
    }

    pub fn with_profit_target(mut self, pct: f64) -> Self {
        self.profit_target_pct = pct; self
    }

    pub fn update_peak(&mut self, current: f64) {
        if current > self.peak_capital { self.peak_capital = current; }
    }

    pub fn record_daily_return(&mut self, val: f64) {
        if self.prev_snapshot > f64::EPSILON {
            self.daily_returns.push((val - self.prev_snapshot) / self.prev_snapshot);
        }
        self.prev_snapshot = val;
    }

    pub fn drawdown_pct(&self, current: f64) -> f64 {
        if self.peak_capital > 0.0 {
            (self.peak_capital - current) / self.peak_capital * 100.0
        } else { 0.0 }
    }

    // [T1] Position size adjusted by regime + RSI + TrendSignal
    pub fn position_size(&self, capital: f64, state: &MarketState) -> f64 {
        let base = capital * self.max_single_trade;
        let rf   = match state.regime { Regime::High => 0.50, Regime::Medium => 0.75, Regime::Low => 1.00 };
        let rsif = if state.rsi > 75.0 || state.rsi < 25.0 { 0.60 } else { 1.00 };
        let tf   = state.trend_signal.position_mult();
        (base * rf * rsif * tf * 10_000.0).round() / 10_000.0
    }

    // Live evaluation — returns TradingStatus
    pub fn evaluate_live(&mut self, current: f64, state: &MarketState) -> (TradingStatus, String) {
        self.update_peak(current);
        let loss = (self.initial_capital - current) / self.initial_capital * 100.0;
        let dd   = self.drawdown_pct(current);
        let pft  = (current - self.initial_capital) / self.initial_capital * 100.0;

        if dd >= self.safety_dd_pct {
            let r = format!("🔴 SAFETY SWITCH: DD {:.2}% ≥ {:.1}%", dd, self.safety_dd_pct);
            return (TradingStatus::SafetySwitchTripped { reason: r.clone() }, r);
        }
        let pause = Instant::now() + std::time::Duration::from_secs(300);
        if loss >= self.max_loss_pct {
            let r = format!("🛑 Stop Loss {:.2}%", loss);
            return (TradingStatus::PausedUntil(pause), r);
        }
        if dd >= self.max_drawdown_pct {
            let r = format!("🛑 Max Drawdown {:.2}%", dd);
            return (TradingStatus::PausedUntil(pause), r);
        }
        if pft >= self.profit_target_pct {
            let r = format!("✅ Take Profit {:.2}%", pft);
            return (TradingStatus::PausedUntil(pause), r);
        }
        if matches!(state.trend_signal, TrendSignal::Bearish)
            && matches!(state.regime, Regime::High)
        {
            let r = "⚠️ Bearish + High Vol".to_string();
            return (TradingStatus::PausedUntil(
                Instant::now() + std::time::Duration::from_secs(60)
            ), r);
        }
        (TradingStatus::Active, String::new())
    }

    // [FIX-7] Backtest evaluation — returns BacktestStatus with tick offset
    pub fn evaluate_backtest(&mut self, current: f64, tick: u64, pause_ticks: u64)
        -> (BacktestStatus, String)
    {
        self.update_peak(current);
        let loss = (self.initial_capital - current) / self.initial_capital * 100.0;
        let dd   = self.drawdown_pct(current);
        let pft  = (current - self.initial_capital) / self.initial_capital * 100.0;

        if dd >= self.safety_dd_pct {
            let r = format!("🔴 SAFETY SWITCH: DD {:.2}%", dd);
            return (BacktestStatus::Stopped(r.clone()), r);
        }
        if loss >= self.max_loss_pct {
            let r = format!("🛑 Stop Loss {:.2}%", loss);
            return (BacktestStatus::PausedUntilTick(tick + pause_ticks), r);
        }
        if dd >= self.max_drawdown_pct {
            let r = format!("🛑 Max Drawdown {:.2}%", dd);
            return (BacktestStatus::PausedUntilTick(tick + pause_ticks), r);
        }
        if pft >= self.profit_target_pct {
            let r = format!("✅ Take Profit {:.2}%", pft);
            return (BacktestStatus::PausedUntilTick(tick + pause_ticks), r);
        }
        (BacktestStatus::Active, String::new())
    }
}

// ══════════════════════════════════════════════════════════
//  [V1] DYNAMIC GRID — fully ATR-relative
//  Works on BTC, ETH, BNB, any symbol automatically
// ══════════════════════════════════════════════════════════
pub struct DynamicGrid {
    pub levels:      Vec<GridLevel>,
    pub build_atr:   f64,
    pub build_price: f64,
    pub grid_levels: usize,
}

impl DynamicGrid {
    pub fn build(state: &MarketState, grid_levels: usize) -> Self {
        // [V1] ATR-relative step
        let regime_mult = match state.regime {
            Regime::High   => 2.5,
            Regime::Medium => 1.8,
            Regime::Low    => 1.2,
        };
        let signal_mult = state.trend_signal.spacing_mult();
        let step = state.atr * regime_mult * signal_mult / (grid_levels as f64 / 2.0);

        // [T1] Bias center toward trend
        let center = match &state.trend_signal {
            TrendSignal::Bullish => state.price * 1.003,
            TrendSignal::Bearish => state.price * 0.997,
            TrendSignal::Caution => match state.trend {
                Trend::Up      => state.price * 1.005,
                Trend::Down    => state.price * 0.995,
                Trend::Neutral => state.price,
            },
        };

        let mut levels = Vec::with_capacity(grid_levels);
        let half = grid_levels / 2;

        for i in 1..=half {
            let p = ((center - i as f64 * step) * 10_000.0).round() / 10_000.0;
            if p > 0.0 {
                levels.push(GridLevel {
                    price:      p,
                    side:       Side::Buy,
                    profit_pct: step / p * 100.0,
                    distance:   (p - state.price).abs() / state.price * 100.0,
                    atr_dist:   (i as f64 * step) / state.atr,
                });
            }
        }
        for i in 1..=half {
            let p = ((center + i as f64 * step) * 10_000.0).round() / 10_000.0;
            levels.push(GridLevel {
                price:      p,
                side:       Side::Sell,
                profit_pct: step / p * 100.0,
                distance:   (p - state.price).abs() / state.price * 100.0,
                atr_dist:   (i as f64 * step) / state.atr,
            });
        }

        Self { levels, build_atr: state.atr, build_price: state.price, grid_levels }
    }

    pub fn needs_rebuild(&self, state: &MarketState, threshold_pct: f64) -> (bool, &'static str) {
        if self.build_atr > f64::EPSILON {
            let chg = (state.atr - self.build_atr).abs() / self.build_atr * 100.0;
            if chg >= threshold_pct { return (true, "ATR shift"); }
        }
        if !self.levels.is_empty() {
            let min_p = self.levels.iter().map(|l| l.price).fold(f64::INFINITY, f64::min);
            let max_p = self.levels.iter().map(|l| l.price).fold(f64::NEG_INFINITY, f64::max);
            if state.price < min_p * 0.995 || state.price > max_p * 1.005 {
                return (true, "price out of grid");
            }
        }
        if regime_bucket(state.atr, state.price) != regime_bucket(self.build_atr, self.build_price) {
            return (true, "regime change");
        }
        (false, "")
    }

    pub fn print_grid(&self, state: &MarketState) {
        info!("{}", "─".repeat(78));
        info!(
            "  {:>10}  {:>5}  {:>9}  {:>11}  {:>7}  {}  ATR:{:.5}",
            "PRICE", "SIDE", "DIST%", "PROFIT%", "ATR×",
            state.trend_signal.label(), self.build_atr
        );
        info!("{}", "─".repeat(78));
        let mut sorted = self.levels.clone();
        sorted.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
        for lv in &sorted {
            let m = if (lv.price - state.price).abs() / state.price < 0.002 { " ◄" } else { "" };
            info!(
                "  {:>10.4}  {:>5}  {:>8.2}%  {:>10.3}%  {:>5.2}×{}",
                lv.price,
                if lv.side == Side::Buy { "BUY" } else { "SELL" },
                lv.distance, lv.profit_pct, lv.atr_dist, m
            );
        }
        info!("{}", "─".repeat(78));
    }
}

// ══════════════════════════════════════════════════════════
//  [W1] VARIABLE SLIPPAGE
//  exec deviation = spread + base_impact × wick_mult + noise
// ══════════════════════════════════════════════════════════
pub fn apply_slippage(
    price:      f64,
    side:       &Side,
    spread_bps: f64,
    trade_val:  f64,
    base_slip:  f64,
    wick_mult:  f64,
) -> (f64, f64) {
    let half_spread = price * spread_bps / 20_000.0;
    let impact_f    = (trade_val / 1_000.0).clamp(0.01, 1.0);
    let base_impact = price * base_slip * impact_f;
    let wick_pen    = base_impact * (wick_mult - 1.0);
    let noise_sign  = if price.fract() > 0.5 { 1.0_f64 } else { -1.0_f64 };
    let noise       = base_impact * 0.2 * noise_sign;
    let total       = half_spread + base_impact + wick_pen + noise;

    let exec = match side {
        Side::Buy  => price + total,
        Side::Sell => price - total,
    };
    (exec.max(0.0001), total.abs())
}

// ══════════════════════════════════════════════════════════
//  PAPER TRADER
//  [FIX-6] pop_front() instead of drain(0..1000)
//  [FIX-8] unified — not duplicated in main.rs
// ══════════════════════════════════════════════════════════
pub struct PaperTrader {
    pub capital:    f64,
    pub base_held:  f64,
    pub trades:     VecDeque<TradeRecord>,
    max_history:    usize,
    pub grid_id:    u64,
    pub filled_buys:  HashSet<u64>,
    pub filled_sells: HashSet<u64>,
    pub total_slip:   f64,
    pub counter:      u64,
    pub symbol:       String,
}

impl PaperTrader {
    pub fn new(capital: f64, max_history: usize, symbol: &str) -> Self {
        Self {
            capital, base_held: 0.0,
            trades:       VecDeque::with_capacity(1000),
            max_history,
            grid_id:      0,
            filled_buys:  HashSet::new(),
            filled_sells: HashSet::new(),
            total_slip:   0.0,
            counter:      0,
            symbol:       symbol.to_string(),
        }
    }

    #[inline]
    fn key(price: f64) -> u64 { (price * 10_000.0).round() as u64 }

    pub fn reset_for_grid(&mut self, grid: &DynamicGrid) {
        let id: u64 = grid.levels.iter()
            .map(|l| Self::key(l.price))
            .fold(0u64, |acc, k| acc ^ k.wrapping_mul(2_654_435_761));
        if id != self.grid_id {
            self.grid_id = id;
            self.filled_buys.clear();
            self.filled_sells.clear();
            info!("  🔄 Grid reset — new id={}", id);
        }
    }

    // [FIX-6] O(1) eviction
    fn push_trade(&mut self, r: TradeRecord) {
        while self.trades.len() >= self.max_history {
            self.trades.pop_front();   // O(1) VecDeque
        }
        self.trades.push_back(r);
    }

    pub fn simulate_fills(
        &mut self,
        grid:         &DynamicGrid,
        price:        f64,
        pos_size:     f64,
        state:        &MarketState,
        min_notional: f64,
        base_slip:    f64,
    ) -> Vec<TradeRecord> {
        self.reset_for_grid(grid);

        let wick_mult = state.wick_profile.slippage_mult();
        let mut records = Vec::new();

        for lv in &grid.levels {
            let key = Self::key(lv.price);

            if lv.side == Side::Buy
                && price <= lv.price
                && !self.filled_buys.contains(&key)
            {
                // [T1] Don't buy in severe downtrend below EMA50
                if price < state.ema50 * 0.97 { continue; }
                let cost = pos_size.min(self.capital);
                if cost < min_notional { continue; }

                let (exec_p, slip) = apply_slippage(
                    lv.price, &Side::Buy, state.spread_bps, cost, base_slip, wick_mult
                );
                let qty = cost / exec_p;
                self.capital   -= cost;
                self.base_held += qty;
                self.total_slip += slip;
                self.filled_buys.insert(key);
                self.filled_sells.remove(&key);
                self.counter += 1;

                let r = TradeRecord {
                    id:          Uuid::new_v4().to_string(),
                    time:        Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    side:        Side::Buy,
                    symbol:      self.symbol.clone(),
                    price:       lv.price,
                    exec_price:  exec_p,
                    qty,
                    value_usdt:  cost,
                    slippage:    slip,
                    slip_mult:   wick_mult,
                    trend_signal: state.trend_signal.label().to_string(),
                    binance_oid: None,
                };
                info!("  📋 BUY #{:04} order={:.4} exec={:.4} qty={:.6} cost=${:.2} slip=${:.5} {}",
                    self.counter, lv.price, exec_p, qty, cost, slip, state.trend_signal.label());
                records.push(r);

            } else if lv.side == Side::Sell
                && price >= lv.price
                && !self.filled_sells.contains(&key)
            {
                if self.base_held < f64::EPSILON { continue; }
                let qty = self.base_held.min(pos_size / lv.price);
                let val = qty * lv.price;
                if val < min_notional { continue; }

                let (exec_p, slip) = apply_slippage(
                    lv.price, &Side::Sell, state.spread_bps, val, base_slip, wick_mult
                );
                let rev = qty * exec_p;
                self.capital    += rev;
                self.base_held  -= qty;
                self.total_slip += slip;
                self.filled_sells.insert(key);
                self.filled_buys.remove(&key);
                self.counter += 1;

                let r = TradeRecord {
                    id:          Uuid::new_v4().to_string(),
                    time:        Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    side:        Side::Sell,
                    symbol:      self.symbol.clone(),
                    price:       lv.price,
                    exec_price:  exec_p,
                    qty,
                    value_usdt:  rev,
                    slippage:    slip,
                    slip_mult:   wick_mult,
                    trend_signal: state.trend_signal.label().to_string(),
                    binance_oid: None,
                };
                info!("  📋 SELL #{:04} order={:.4} exec={:.4} qty={:.6} rev=${:.2} slip=${:.5} {}",
                    self.counter, lv.price, exec_p, qty, rev, slip, state.trend_signal.label());
                records.push(r);
            }
        }

        for r in &records { self.push_trade(r.clone()); }
        records
    }

    pub fn total_value(&self, price: f64) -> f64 { self.capital + self.base_held * price }
    pub fn pnl_pct(&self, price: f64) -> f64 {
        let init = self.capital + self.base_held * price
            - self.trades.iter().map(|t|
                if t.side == Side::Buy { t.value_usdt } else { -t.value_usdt }
            ).sum::<f64>();
        (self.total_value(price) - init) / init.max(1.0) * 100.0
    }

    pub fn stats(&self) -> (u64, usize, usize, f64, f64) {
        let b = self.trades.iter().filter(|t| t.side == Side::Buy).count();
        let s = self.trades.iter().filter(|t| t.side == Side::Sell).count();
        let v = self.trades.iter().map(|t| t.value_usdt).sum::<f64>();
        (self.counter, b, s, v, self.total_slip)
    }
}

// ══════════════════════════════════════════════════════════
//  [O1] AUTO-OPTIMIZER — 5 profit_target values
//  Reports Sharpe + Calmar per run
// ══════════════════════════════════════════════════════════

pub struct BtRun {
    pub final_val:    f64,
    pub max_dd:       f64,
    pub total_trades: usize,
    pub daily_rets:   Vec<f64>,
}

/// Fast backtest kernel for optimizer
/// [FIX-5] Uses real ATR from OHLC when available
pub fn backtest_kernel(
    candles:       &[crate::models::Candle],
    prices:        &[f64],
    cfg:           &BotConfig,
    profit_target: f64,
) -> BtRun {
    let cap = cfg.total_capital;
    let mut usdt   = cap;
    let mut base   = 0.0_f64;
    let mut peak   = cap;
    let mut max_dd = 0.0_f64;
    let mut trades = 0_usize;
    let mut buys_h:  HashSet<u64> = HashSet::new();
    let mut sells_h: HashSet<u64> = HashSet::new();
    let mut daily_rets = Vec::<f64>::new();
    let mut day_val    = cap;

    let atr_period = 14_usize;

    for (i, &price) in prices.iter().enumerate() {
        // [FIX-5] Real ATR from OHLC slice
        let atr = if i + 1 >= atr_period && i < candles.len() {
            let slice = &candles[(i + 1).saturating_sub(atr_period)..=i.min(candles.len()-1)];
            crate::analyzer::MarketAnalyzer::calc_atr(slice, atr_period.min(slice.len().saturating_sub(1)), cfg.atr_outlier_cap_pct)
                .max(price * 0.003) // floor at 0.3%
        } else {
            price * 0.005  // fallback for warmup period only
        };

        let step = atr * 1.8 / (cfg.grid_levels as f64 / 2.0);

        // Rebuild grid every N ticks
        let rebuild = i % cfg.grid_rebuild_ticks as usize == 0;
        if rebuild { buys_h.clear(); sells_h.clear(); }

        let pos = cap * cfg.max_single_trade_pct;

        for lv_i in 1..=(cfg.grid_levels / 2) {
            // BUY levels
            let gp   = price - lv_i as f64 * step;
            let bkey = (gp * 10_000.0).round() as u64;
            if !buys_h.contains(&bkey) && usdt >= pos {
                let exec = gp * (1.0 + cfg.slippage_pct);
                let qty  = pos / exec;
                usdt -= pos; base += qty;
                buys_h.insert(bkey);
                trades += 1;
            }
            // SELL levels
            let sp   = price + lv_i as f64 * step;
            let skey = (sp * 10_000.0).round() as u64 + 1_000_000_000;
            if !sells_h.contains(&skey) && base > 0.0 {
                let exec   = sp * (1.0 - cfg.slippage_pct);
                let sell_q = base.min(pos / sp);
                usdt += sell_q * exec;
                base -= sell_q;
                sells_h.insert(skey);
                trades += 1;
            }
        }

        let val = usdt + base * price;
        if val > peak { peak = val; }
        let dd = (peak - val) / peak * 100.0;
        if dd > max_dd { max_dd = dd; }

        // Profit target / safety switch
        let pft = (val - cap) / cap * 100.0;
        if pft >= profit_target || dd >= cfg.max_drawdown_pct * 1.5 { break; }

        // Daily return snapshot every 1440 ticks
        if i % 1440 == 0 && i > 0 {
            daily_rets.push((val - day_val) / day_val.max(1e-9));
            day_val = val;
        }
    }

    let final_val = usdt + base * prices.last().copied().unwrap_or(0.0);
    BtRun { final_val, max_dd, total_trades: trades, daily_rets }
}

fn sharpe(rets: &[f64]) -> f64 {
    if rets.len() < 2 { return 0.0; }
    let n   = rets.len() as f64;
    let avg = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - avg).powi(2)).sum::<f64>() / (n - 1.0);
    let std = var.sqrt();
    if std < 1e-12 { return 0.0; }
    avg / std * 252_f64.sqrt()
}

pub fn auto_optimize(
    candles: &[crate::models::Candle],
    prices:  &[f64],
    cfg:     &BotConfig,
) -> Vec<OptResult> {
    let targets = [0.5_f64, 1.0, 2.0, 5.0, 10.0];
    let mut results = Vec::with_capacity(targets.len());

    info!("╔══════════════════════════════════════════════════════════╗");
    info!("║      [O1] AUTO-OPTIMIZER — Sharpe + Calmar sweep         ║");
    info!("╚══════════════════════════════════════════════════════════╝");
    info!("  {:>14}  {:>12}  {:>9}  {:>10}  {:>10}  {:>10}",
        "profit_target", "final_val", "pnl%", "trades", "sharpe", "calmar");
    info!("{}", "─".repeat(80));

    for &t in &targets {
        let bt  = backtest_kernel(candles, prices, cfg, t);
        let pnl = (bt.final_val - cfg.total_capital) / cfg.total_capital * 100.0;
        let sh  = sharpe(&bt.daily_rets);
        let cal = if bt.max_dd > f64::EPSILON { pnl / bt.max_dd } else { 0.0 };

        info!("  {:>13.1}%  {:>12.2}  {:>8.3}%  {:>10}  {:>10.4}  {:>10.4}",
            t, bt.final_val, pnl, bt.total_trades, sh, cal);

        results.push(OptResult {
            profit_target_pct: t,
            final_value:       bt.final_val,
            net_pnl_pct:       pnl,
            total_trades:      bt.total_trades,
            max_drawdown_pct:  bt.max_dd,
            sharpe_ratio:      sh,
            calmar_ratio:      cal,
        });
    }

    if let Some(best) = results.iter()
        .max_by(|a, b| a.sharpe_ratio.partial_cmp(&b.sharpe_ratio)
            .unwrap_or(std::cmp::Ordering::Equal))
    {
        info!("  🏆 Best: profit_target={:.1}%  sharpe={:.4}  pnl={:.3}%  dd={:.2}%",
            best.profit_target_pct, best.sharpe_ratio, best.net_pnl_pct, best.max_drawdown_pct);
    }

    results
}
