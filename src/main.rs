// ╔══════════════════════════════════════════════════════════════════════╗
// ║       SPD SOFTWARE LAB — Adaptive Intelligent Grid Bot  v3.0        ║
// ╠══════════════════════════════════════════════════════════════════════╣
// ║  [FIX-8] Unified — main.rs imports from modules, NO duplicates      ║
// ║  [FIX-2] TradingStatus.is_active() — no PartialEq on Instant        ║
// ║  [FIX-5] Real ATR from OHLC in backtest                             ║
// ║  [FIX-6] pop_front() O(1) eviction                                  ║
// ║  [FIX-7] BacktestStatus uses tick counter, not Instant              ║
// ║  [T1]    EMA 9/21 TrendSignal                                       ║
// ║  [V1]    ATR-relative grid (any symbol)                             ║
// ║  [W1]    Variable slippage via WickProfile                          ║
// ║  [O1]    Auto-optimizer (Sharpe + Calmar)                           ║
// ║  [L1]    Daily rotating log file                                    ║
// ║  LIVE:   Real Binance orders via HMAC-signed REST                   ║
// ║  COPY:   HTTP server broadcasts signals to followers                ║
// ╚══════════════════════════════════════════════════════════════════════╝

// [FIX-8] All types come from modules — no redefinition in main.rs
mod models;
mod config;
mod analyzer;
mod engine;
mod network;
mod trader;
mod copytrade;

use std::collections::VecDeque;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Local;
use tracing::{error, info, warn};
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use config::BotConfig;
use engine::{auto_optimize, BacktestStatus, DynamicGrid, PaperTrader, RiskManager};
use models::OptResult; // استيراد مباشر
use analyzer::{latency_stats, MarketAnalyzer};
use models::{Candle, TradingStatus};
use network::ws_price_listener;
use trader::{BinanceTrader, user_data_stream};
use copytrade::{CopyBroadcast, CopyTradeState, start_server};
use tokio::sync::{watch, Mutex};

// ══════════════════════════════════════════════════════════
//  [L1] Logging — terminal (coloured) + daily rotating file
// ══════════════════════════════════════════════════════════
fn init_logging() {
    fs::create_dir_all("logs").ok();
    let file_appender            = rolling::daily("logs", "spd-bot.log");
    let (non_blocking, _guard)   = tracing_appender::non_blocking(file_appender);

    let stdout = fmt::layer().with_target(false).with_ansi(true);
    let file   = fmt::layer().with_target(true).with_ansi(false).with_writer(non_blocking);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info")))
        .with(stdout)
        .with(file)
        .init();

    std::mem::forget(_guard); // keep guard alive for program lifetime
}

// ══════════════════════════════════════════════════════════
//  Session report (printed + logged at shutdown / end)
// ══════════════════════════════════════════════════════════
fn session_report(
    paper:       &PaperTrader,
    risk:        &RiskManager,
    initial:     f64,
    final_price: f64,
    tick:        u64,
    elapsed:     Duration,
    opt:         &[OptResult],
    symbol:      &str,
) {
    let val    = paper.total_value(final_price);
    let pnl    = val - initial;
    let pct    = pnl / initial * 100.0;
    let max_dd = risk.drawdown_pct(val);  // [L1] instantaneous at close
    let (tot, b, s, vol, slip) = paper.stats();

    info!("╔══════════════════════════════════════════════════════════════╗");
    info!("║            SPD Grid Bot — SESSION REPORT                     ║");
    info!("╠══════════════════════════════════════════════════════════════╣");
    info!("  Symbol           : {}", symbol);
    info!("  Start Capital    : ${:.2}", initial);
    info!("  Final Portfolio  : ${:.2}", val);
    info!("  Net PnL          : ${:+.2}  ({:+.3}%)", pnl, pct);
    info!("  Max Drawdown NOW : {:.3}%", max_dd);
    info!("  Peak Capital     : ${:.2}", risk.peak_capital);
    info!("  ─────────────────────────────────────────────────────────");
    info!("  Total Trades     : {}", tot);
    info!("  Buys / Sells     : {} / {}", b, s);
    info!("  Volume           : ${:.2}", vol);
    info!("  Total Slippage   : ${:.4}", slip);
    info!("  ─────────────────────────────────────────────────────────");
    info!("  Ticks            : {}", tick);
    info!("  Elapsed          : {:.1}s", elapsed.as_secs_f64());

    if !opt.is_empty() {
        info!("  ─────────────────────────────────────────────────────────");
        info!("  [O1] Optimizer:");
        info!("  {:>14}  {:>9}  {:>10}  {:>10}", "target%", "pnl%", "sharpe", "calmar");
        let best_sh = opt.iter()
            .max_by(|a,b| a.sharpe_ratio.partial_cmp(&b.sharpe_ratio).unwrap())
            .map(|r| r.profit_target_pct).unwrap_or(0.0);
        for r in opt {
            let m = if r.profit_target_pct == best_sh { " ←BEST" } else { "" };
            info!("  {:>13.1}%  {:>8.3}%  {:>10.4}  {:>10.4}{}",
                r.profit_target_pct, r.net_pnl_pct, r.sharpe_ratio, r.calmar_ratio, m);
        }
    }
    info!("════════════════════════════════════════════════════════════════");
}

// ══════════════════════════════════════════════════════════
//  BACKTEST MODE
//  [FIX-5] Loads OHLC from data.json for real ATR
//  [FIX-7] BacktestStatus uses tick offset, not Instant
// ══════════════════════════════════════════════════════════
async fn run_backtest(cfg: BotConfig) {
    info!("📂 Loading data.json...");

    let raw = match fs::read_to_string("data.json") {
        Ok(d)  => d,
        Err(e) => { error!("data.json: {}", e); return; }
    };

    // Binance kline format: [[open_time, open, high, low, close, ...], ...]
    let raw_data: Vec<Vec<serde_json::Value>> = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(e) => { error!("JSON: {}", e); return; }
    };

    // [FIX-1] Build full Candle structs with open for WickProfile + real ATR
    let candles: Vec<Candle> = raw_data.iter().filter_map(|k| {
        let open  = k.get(1)?.as_str()?.parse::<f64>().ok()?;
        let high  = k.get(2)?.as_str()?.parse::<f64>().ok()?;
        let low   = k.get(3)?.as_str()?.parse::<f64>().ok()?;
        let close = k.get(4)?.as_str()?.parse::<f64>().ok()?;
        if high < low || close <= 0.0 { return None; }
        Some(Candle { open, high, low, close })
    }).collect();

    let prices: Vec<f64> = candles.iter().map(|c| c.close).collect();

    if prices.is_empty() { error!("No valid prices"); return; }
    info!("  ✅ Loaded {} candles", candles.len());

    // [O1] Auto-optimize first
    info!("\n🔍 Running Auto-Optimizer...");
    let opt_results = auto_optimize(&candles, &prices, &cfg);

    // Full backtest with configured target
    info!("\n🚀 Backtest ({} ticks)...", prices.len());
    let start = Instant::now();

    let mut paper  = PaperTrader::new(cfg.total_capital, cfg.max_trade_history, &cfg.symbol);
    let mut risk   = RiskManager::new(&cfg);
    let mut status = BacktestStatus::Active;

    let mut last_rebuild = 0_u64;

    for (idx, &price) in prices.iter().enumerate() {
        let tick = idx as u64 + 1;

        // [FIX-7] Pause by tick counter — not Instant
        if let BacktestStatus::PausedUntilTick(until) = &status {
            if tick < *until { continue; }
            status = BacktestStatus::Active;
        }
        if status.is_stopped() { break; }

        // [FIX-5] Real ATR from preceding candles
        let atr_start = idx.saturating_sub(14);
        let atr_slice = &candles[atr_start..=idx];
        let atr = if atr_slice.len() > 1 {
            MarketAnalyzer::calc_atr(atr_slice, atr_slice.len() - 1, cfg.atr_outlier_cap_pct)
                .max(price * 0.003)
        } else {
            price * 0.005
        };

        // Build minimal MarketState for the engine
        let ema9  = price; // simplified for backtest speed
        let ema21 = price;
        let state = models::MarketState {
            price, atr,
            rsi: 50.0,
            ema9, ema20: price, ema21, ema50: price,
            trend:        models::Trend::Neutral,
            trend_signal: models::TrendSignal::Caution,
            regime: if atr/price < 0.008 { models::Regime::Low }
                    else if atr/price < 0.020 { models::Regime::Medium }
                    else { models::Regime::High },
            spread_bps: 5.0,
            // [W1] Real wick profile from last 20 candles
            wick_profile: models::WickProfile::from_candles(
                &candles[idx.saturating_sub(20)..=idx]
            ),
        };

        // Rebuild grid
        let force_rebuild = tick == 1 || (tick - last_rebuild) >= cfg.grid_rebuild_ticks;
        let atr_chg = if last_rebuild > 0 {
            (atr - price * 0.005).abs() / (price * 0.005) * 100.0
        } else { 100.0 };

        static GRID: std::sync::OnceLock<std::sync::Mutex<Option<DynamicGrid>>> =
            std::sync::OnceLock::new();
        let grid_lock = GRID.get_or_init(|| std::sync::Mutex::new(None));

        if force_rebuild || atr_chg > cfg.atr_rebuild_threshold_pct {
            *grid_lock.lock().unwrap() = Some(DynamicGrid::build(&state, cfg.grid_levels));
            last_rebuild = tick;
        }

        let grid_guard = grid_lock.lock().unwrap();
        if let Some(grid) = grid_guard.as_ref() {
            let pos = risk.position_size(paper.capital, &state);
            paper.simulate_fills(grid, price, pos, &state, cfg.min_notional_usdt, cfg.slippage_pct);
        }
        drop(grid_guard);

        let val = paper.total_value(price);
        let (new_status, reason) = risk.evaluate_backtest(val, tick, cfg.grid_rebuild_ticks * 2);

        if !reason.is_empty() { warn!("  [tick {}] {}", tick, reason); }
        status = new_status;

        // Daily return snapshot
        if tick % 1440 == 0 { risk.record_daily_return(val); }

        if tick % 50_000 == 0 {
            info!("  [tick {:>7}] P:{:.4}  Val:${:.2}  Trades:{}",
                tick, price, val, paper.counter);
        }
    }

    let final_price = *prices.last().unwrap_or(&0.0);
    session_report(
        &paper, &risk, cfg.total_capital,
        final_price, prices.len() as u64,
        start.elapsed(), &opt_results, &cfg.symbol,
    );
}

// ══════════════════════════════════════════════════════════
//  LIVE / PAPER TRADING MODE
// ══════════════════════════════════════════════════════════
async fn run_live(cfg: BotConfig) {
    let is_live = cfg.is_live();

    // [LIVE] Create Binance trader
    let trader = if is_live {
        Some(Arc::new(BinanceTrader::new(
            cfg.api_key(), cfg.api_secret(),
            cfg.symbol.clone(), false,
        )))
    } else { None };

    // Copy-trade setup
    let copy_state = CopyTradeState::new(cfg.master_id.clone(), cfg.fee_pct);
    let broadcaster = Arc::new(CopyBroadcast::new(copy_state.clone()));

    if cfg.copy_trade_enabled {
        let state_clone = copy_state.clone();
        let port        = cfg.copy_trade_port;
        tokio::spawn(async move { start_server(state_clone, port).await });
    }

    // WebSocket price feed
    let latencies  = Arc::new(Mutex::new(VecDeque::<u64>::with_capacity(cfg.latency_buf_size)));
    let (price_tx, mut price_rx) = watch::channel::<Option<f64>>(None);

    let sym_lower  = cfg.symbol_lower();
    let lat_clone  = Arc::clone(&latencies);
    tokio::spawn(async move {
        ws_price_listener(price_tx, lat_clone, sym_lower, 100).await;
    });

    // [LIVE] User Data Stream for fill confirmation
    if let Some(t) = &trader {
        match t.create_listen_key().await {
            Ok(key) => {
                let (fill_tx, mut fill_rx) = tokio::sync::mpsc::channel(100);
                let key_clone   = key.clone();
                tokio::spawn(async move {
                    user_data_stream(key_clone, fill_tx).await;
                });
                tokio::spawn(async move {
                    while let Some(report) = fill_rx.recv().await {
                        info!("  ✅ [FILL] orderId={} side={} qty={} price={}",
                            report.order_id, report.side, report.last_qty, report.last_price);
                    }
                });
                // Keepalive every 25 min
                let t_clone = Arc::clone(t);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(1500)).await;
                        t_clone.keepalive_listen_key(&key).await;
                    }
                });
            }
            Err(e) => warn!("[live] Failed to create listenKey: {}", e),
        }
    }

    info!("  ⏳ Waiting for first price...");
    loop {
        price_rx.changed().await.ok();
        if price_rx.borrow().is_some() { break; }
    }
    info!("  ✅ First price received — starting");

    let analyzer  = MarketAnalyzer::new(cfg.rate_limit_per_sec);
    let mut risk  = RiskManager::new(&cfg);
    let mut paper = PaperTrader::new(cfg.total_capital, cfg.max_trade_history, &cfg.symbol);

    let mut state:        Option<Arc<models::MarketState>> = None;
    let mut grid:         Option<DynamicGrid>               = None;
    let mut tick          = 0_u64;
    let mut cycle         = 0_u64;
    let mut paused_until: Option<Instant>                   = None;
    let mut opt_results   = Vec::<OptResult>::new();
    let start_time        = Instant::now();

    // Heartbeat
    let hb = cfg.heartbeat_secs;
    tokio::spawn(async move {
        loop { tokio::time::sleep(Duration::from_secs(hb)).await; info!("  💓 heartbeat"); }
    });

    loop {
        if price_rx.changed().await.is_err() {
            error!("Price channel closed"); break;
        }

        let price = match *price_rx.borrow() { Some(p) => p, None => continue };
        tick += 1;

        // ── Rebuild every N ticks ──────────────────────────────
        if tick % cfg.grid_rebuild_ticks == 1 {
            cycle += 1;
            info!("\n{} Cycle #{} tick={} {}", "═".repeat(10), cycle, tick, "═".repeat(10));

            let mut new_state = None;
            for attempt in 0..cfg.max_retries {
                new_state = analyzer.analyze(&cfg.symbol, price, cfg.atr_outlier_cap_pct).await;
                if new_state.is_some() { break; }
                let w = cfg.retry_base_sec * 2u64.pow(attempt);
                warn!("  retry {}/{} in {}s", attempt+1, cfg.max_retries, w);
                tokio::time::sleep(Duration::from_secs(w)).await;
            }

            if let Some(s) = new_state {
                // Run optimizer once
                if cycle == 1 && !prices_for_opt.is_empty() {
                    // (optimizer runs in backtest mode; skip in live to save API calls)
                }

                info!("  📊 P:{:.4} ATR:{:.5} RSI:{:.1} EMA9:{:.4} EMA21:{:.4} {} Wick:{:.3}",
                    s.price, s.atr, s.rsi, s.ema9, s.ema21,
                    s.trend_signal.label(), s.wick_profile.avg_wick_ratio);

                let g = DynamicGrid::build(&s, cfg.grid_levels);
                g.print_grid(&s);

                info!("  🧭 {} pos×{:.2} space×{:.2}",
                    s.trend_signal.label(),
                    s.trend_signal.position_mult(),
                    s.trend_signal.spacing_mult());

                // Latency
                let lat = latencies.lock().await;
                if let Some((avg,mn,mx,std)) = latency_stats(&lat) {
                    info!("  📡 WS: avg={}ms min={}ms max={}ms σ={:.1}ms", avg,mn,mx,std);
                }

                state = Some(Arc::new(s));
                grid  = Some(g);
            }
        }

        // ── Stale price guard ──────────────────────────────────
        // (price is always fresh from the watch channel — if channel is dead we broke above)

        // ── Pause guard ────────────────────────────────────────
        if let Some(until) = paused_until {
            if Instant::now() < until { continue; }
            paused_until = None;
            info!("  ▶️  Resuming");
        }

        let s = match state.as_ref() { Some(s) => Arc::clone(s), None => continue };
        let g = match grid.as_ref()  { Some(g) => g, None => continue };

        // ── Risk check ──────────────────────────────────────────
        let val = paper.total_value(price);
        let (new_st, reason) = risk.evaluate_live(val, &s);

        match &new_st {
            TradingStatus::SafetySwitchTripped { reason } => {
                error!("  {}", reason); break;
            }
            TradingStatus::PausedUntil(until) => {
                if !reason.is_empty() { warn!("  {} — paused 5min", reason); }
                paused_until = Some(*until);
                continue;
            }
            TradingStatus::Active => {}
        }

        // ── Execute fills ───────────────────────────────────────
        let pos  = risk.position_size(paper.capital, &s);
        let recs = paper.simulate_fills(g, price, pos, &s, cfg.min_notional_usdt, cfg.slippage_pct);

        // ── LIVE: Place real orders + broadcast copy signals ────
        for rec in &recs {
            if let Some(t) = &trader {
                // Fire real order (async, don't block the price loop)
                let t_clone = Arc::clone(t);
                let side    = rec.side.clone();
                let ord_p   = rec.price;
                let qty     = rec.qty;
                let sym     = cfg.symbol.clone();
                tokio::spawn(async move {
                    t_clone.execute_grid_trade(&side, ord_p, qty, &sym).await;
                });
            }

            // Broadcast to copy-trade followers
            if cfg.copy_trade_enabled {
                let bc  = Arc::clone(&broadcaster);
                let rec = rec.clone();
                tokio::spawn(async move { bc.broadcast(&rec).await; });
            }
        }

        // ── Status print ───────────────────────────────────────
        if tick % (cfg.status_secs * 10) == 0 {
            let pnl_pct = (val - cfg.total_capital) / cfg.total_capital * 100.0;
            let dd      = risk.drawdown_pct(val);
            let (tot, b, sv, _, _) = paper.stats();
            info!("  💰 P:{:.4}  Val:${:.2}  PnL:{:+.3}%  DD:{:.2}%  T:{}(B:{} S:{})",
                price, val, pnl_pct, dd, tot, b, sv);
        }

        // Daily return snapshot
        if tick % (1440 * 10) == 0 { risk.record_daily_return(val); }
    }

    session_report(
        &paper, &risk, cfg.total_capital,
        price_rx.borrow().unwrap_or(0.0),
        tick, start_time.elapsed(), &opt_results, &cfg.symbol,
    );
}

// Placeholder — live mode doesn't run optimizer from REST data
static prices_for_opt: &[f64] = &[];

// ══════════════════════════════════════════════════════════
//  ENTRY POINT
// ══════════════════════════════════════════════════════════
#[tokio::main]
async fn main() {
    // Load .env (BINANCE_API_KEY, BINANCE_API_SECRET, ADMIN_TOKEN, COPY_TRADE_SECRET)
    dotenv::dotenv().ok();
    init_logging();

    info!("╔══════════════════════════════════════════════════════════╗");
    info!("║        -------------  SPD Grid Bot  -------------        ║");
    info!("╚══════════════════════════════════════════════════════════╝");

    let cfg = BotConfig::load("config.toml").await;

    let errs = cfg.validate();
    if !errs.is_empty() {
        for e in &errs { error!("[CONFIG] {}", e); }
        std::process::exit(1);
    }

    info!("  Mode    : {}", cfg.mode.to_uppercase());
    info!("  Symbol  : {}  Capital: ${:.2}  GridLevels: {}",
        cfg.symbol, cfg.total_capital, cfg.grid_levels);
    info!("  CopyTrade: {}  Fee: {:.0}%  Port: {}",
        cfg.copy_trade_enabled, cfg.fee_pct, cfg.copy_trade_port);

    match cfg.mode.as_str() {
        "backtest" => run_backtest(cfg).await,
        "live" | "paper" | _ => run_live(cfg).await,
    }
}
