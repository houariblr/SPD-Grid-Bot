// ═══════════════════════════════════════════════════════════
//  analyzer.rs — SPD Software Lab  v3.0 PRODUCTION
//  FIXES:
//  [FIX-1] Candle includes open (parsed from k.1)
//  [FIX-5] ATR uses real OHLC True Range (not price * 0.005)
//  [T1]    EMA 9/21 TrendSignal
//  [W1]    WickProfile from real open/high/low/close
// ═══════════════════════════════════════════════════════════

use crate::models::{Candle, MarketState, RawKline, Regime, Trend, TrendSignal, WickProfile};
use crate::network::RateLimiter;
use std::collections::VecDeque;
use tokio::sync::Mutex;
use std::time::Duration;
use tracing::{debug, warn};

pub struct MarketAnalyzer {
    client:       reqwest::Client,
    rate_limiter: Mutex<RateLimiter>,
}

impl MarketAnalyzer {
    pub fn new(rps: u64) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            rate_limiter: Mutex::new(RateLimiter::new(rps)),
        }
    }

    // ─────────────────────────────────────────────────────
    //  Fetch klines from Binance REST
    // ─────────────────────────────────────────────────────
    pub async fn get_candles(
        &self,
        symbol:   &str,
        interval: &str,
        limit:    usize,
        cap_pct:  f64,
    ) -> Result<Vec<Candle>, String> {
        self.rate_limiter.lock().await.acquire().await;

        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol={}&interval={}&limit={}",
            symbol, interval, limit
        );

        let resp = self.client.get(&url).send().await
            .map_err(|e| format!("connect: {}", e))?;

        // Respect Binance weight header
        if let Some(w) = resp.headers()
            .get("x-mbx-used-weight-1m")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
        {
            if w > 1000 {
                warn!("[analyzer] Binance weight {}/1200 — sleeping 5s", w);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }

        match resp.status() {
            s if s == reqwest::StatusCode::TOO_MANY_REQUESTS =>
                return Err("429 Rate limit".into()),
            s if s == reqwest::StatusCode::FORBIDDEN =>
                return Err("403 Forbidden".into()),
            s if !s.is_success() =>
                return Err(format!("HTTP {}", s)),
            _ => {}
        }

        let raw: Vec<RawKline> = resp.json().await
            .map_err(|e| format!("JSON: {}", e))?;

        // [FIX-1] Parse open too — required for WickProfile
        let candles: Vec<Candle> = raw.iter().filter_map(|k| {
            let open  = k.1.parse::<f64>().ok()?;
            let high  = k.2.parse::<f64>().ok()?;
            let low   = k.3.parse::<f64>().ok()?;
            let close = k.4.parse::<f64>().ok()?;
            if high <= 0.0 || low <= 0.0 || close <= 0.0 || low > high { return None; }
            // Cap extreme candles (flash crashes, listing pumps)
            if (high - low) / close * 100.0 > cap_pct * 2.0 { return None; }
            Some(Candle { open, high, low, close })
        }).collect();

        if candles.len() < 21 {
            return Err(format!("Insufficient candles: {}", candles.len()));
        }
        Ok(candles)
    }

    // ─────────────────────────────────────────────────────
    //  [FIX-5] ATR — Wilder's RMA with real True Range
    //  NOT price * 0.005 — uses high/low/prev_close
    // ─────────────────────────────────────────────────────
    pub fn calc_atr(candles: &[Candle], period: usize, cap_pct: f64) -> f64 {
        if candles.len() < period + 1 { return 0.0; }

        let trs: Vec<f64> = candles.windows(2).map(|w| {
            let prev_close = w[0].close;
            let c = &w[1];
            // Real True Range = max(H-L, |H-prev|, |L-prev|)
            let tr = (c.high - c.low)
                .max((c.high - prev_close).abs())
                .max((c.low  - prev_close).abs());
            // Cap outlier spikes
            tr.min(prev_close * cap_pct / 100.0)
        }).collect();

        // Wilder seed = SMA of first `period` TRs
        let mut atr = trs[..period].iter().sum::<f64>() / period as f64;
        // Then Wilder smoothing: ATR = (ATR_prev × (n-1) + TR) / n
        for tr in &trs[period..] {
            atr = (atr * (period as f64 - 1.0) + tr) / period as f64;
        }
        atr
    }

    // ─────────────────────────────────────────────────────
    //  RSI — Wilder's Smoothing
    // ─────────────────────────────────────────────────────
    pub fn calc_rsi(candles: &[Candle], period: usize) -> f64 {
        let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
        if closes.len() < period + 1 { return 50.0; }

        let deltas: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();
        let init = &deltas[..period];

        let mut avg_gain = init.iter().map(|d| d.max(0.0)).sum::<f64>() / period as f64;
        let mut avg_loss = init.iter().map(|d| (-d).max(0.0)).sum::<f64>() / period as f64;

        for d in &deltas[period..] {
            avg_gain = (avg_gain * (period as f64 - 1.0) + d.max(0.0))  / period as f64;
            avg_loss = (avg_loss * (period as f64 - 1.0) + (-d).max(0.0)) / period as f64;
        }

        if avg_loss < f64::EPSILON { return 100.0; }
        100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    }

    // ─────────────────────────────────────────────────────
    //  EMA — SMA seed → exponential (alpha = 2/(n+1))
    // ─────────────────────────────────────────────────────
    pub fn calc_ema(candles: &[Candle], period: usize) -> f64 {
        if candles.len() < period {
            return candles.last().map(|c| c.close).unwrap_or(0.0);
        }
        let alpha = 2.0 / (period as f64 + 1.0);
        let mut ema = candles[..period].iter().map(|c| c.close).sum::<f64>() / period as f64;
        for c in &candles[period..] {
            ema = c.close * alpha + ema * (1.0 - alpha);
        }
        ema
    }

    // ─────────────────────────────────────────────────────
    //  [T1] EMA 9/21 Trend Signal
    // ─────────────────────────────────────────────────────
    pub fn trend_signal(price: f64, ema9: f64, ema21: f64) -> TrendSignal {
        let bull_ema = ema9 > ema21 * 1.0005;
        let bear_ema = ema9 < ema21 * 0.9995;
        if bull_ema && price > ema9 {
            TrendSignal::Bullish
        } else if bear_ema && price < ema9 {
            TrendSignal::Bearish
        } else {
            TrendSignal::Caution
        }
    }

    pub fn regime(atr: f64, price: f64) -> Regime {
        let r = atr / price;
        if r < 0.008 { Regime::Low } else if r < 0.020 { Regime::Medium } else { Regime::High }
    }

    pub fn spread_bps(atr: f64, price: f64, regime: &Regime) -> f64 {
        let base = (atr / price) * 10_000.0 * 0.15;
        let mult = match regime { Regime::Low => 0.8, Regime::Medium => 1.2, Regime::High => 2.5 };
        (base * mult).clamp(0.5, 30.0)
    }

    // ─────────────────────────────────────────────────────
    //  Full analysis (called every rebuild cycle)
    // ─────────────────────────────────────────────────────
    pub async fn analyze(
        &self,
        symbol:     &str,
        live_price: f64,
        cap_pct:    f64,
    ) -> Option<MarketState> {
        let candles = self.get_candles(symbol, "1h", 100, cap_pct).await
            .map_err(|e| { warn!("[analyzer] {}", e); e })
            .ok()?;

        // [FIX-5] Real ATR from OHLC
        let atr    = Self::calc_atr(&candles, 14, cap_pct);
        let rsi    = Self::calc_rsi(&candles, 14);
        let ema9   = Self::calc_ema(&candles,  9);
        let ema20  = Self::calc_ema(&candles, 20);
        let ema21  = Self::calc_ema(&candles, 21);
        let ema50  = Self::calc_ema(&candles, 50);
        let regime = Self::regime(atr, live_price);
        let spread = Self::spread_bps(atr, live_price, &regime);
        let signal = Self::trend_signal(live_price, ema9, ema21);

        let trend = if ema20 > ema50 * 1.005 { Trend::Up }
                    else if ema20 < ema50 * 0.995 { Trend::Down }
                    else { Trend::Neutral };

        // [W1] WickProfile from last 20 candles with real open
        let wp_slice = if candles.len() >= 20 { &candles[candles.len()-20..] } else { &candles[..] };
        let wick_profile = WickProfile::from_candles(wp_slice);

        debug!(
            "[analyze] P:{:.4} ATR:{:.5} RSI:{:.1} EMA9:{:.4} EMA21:{:.4} {} WickR:{:.3}",
            live_price, atr, rsi, ema9, ema21, signal.label(), wick_profile.avg_wick_ratio
        );

        Some(MarketState {
            price: live_price, atr, rsi,
            ema9, ema20, ema21, ema50,
            trend, trend_signal: signal,
            regime, spread_bps: spread,
            wick_profile,
        })
    }
}

// ── Latency statistics (used by main loop) ───────────────────

pub fn latency_stats(buf: &VecDeque<u64>) -> Option<(u64, u64, u64, f64)> {
    if buf.is_empty() { return None; }
    let avg = buf.iter().sum::<u64>() / buf.len() as u64;
    let min = *buf.iter().min().unwrap();
    let max = *buf.iter().max().unwrap();
    let var = buf.iter()
        .map(|&x| { let d = x as i64 - avg as i64; (d * d) as f64 })
        .sum::<f64>() / buf.len() as f64;
    Some((avg, min, max, var.sqrt()))
}
