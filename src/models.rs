// ═══════════════════════════════════════════════════════════
//  models.rs — SPD Software Lab  v3.0 PRODUCTION
//  FIXES:
//  [FIX-1] Candle now has `open` field (required for WickProfile)
//  [FIX-2] TradingStatus has is_active() (Instant !PartialEq)
//  [FIX-3] TrendSignal with position_mult + spacing_mult
//  [FIX-4] WickProfile::slippage_multiplier() for variable slip
// ═══════════════════════════════════════════════════════════

use serde::{Deserialize, Serialize};

// ── WebSocket structs ───────────────────────────────────────

/// aggTrade stream message
#[derive(Deserialize, Debug)]
pub struct WsTrade {
    #[serde(rename = "p")]
    pub price:      String,
    #[serde(rename = "T")]
    pub trade_time: u64,
}

/// Binance order fill event from User Data Stream
#[derive(Deserialize, Debug)]
pub struct WsExecutionReport {
    #[serde(rename = "e")]
    pub event_type: String,        // "executionReport"
    #[serde(rename = "s")]
    pub symbol:     String,
    #[serde(rename = "i")]
    pub order_id:   u64,
    #[serde(rename = "S")]
    pub side:       String,        // "BUY" / "SELL"
    #[serde(rename = "X")]
    pub status:     String,        // "FILLED" / "PARTIALLY_FILLED"
    #[serde(rename = "l")]
    pub last_qty:   String,
    #[serde(rename = "L")]
    pub last_price: String,
    #[serde(rename = "n")]
    pub commission: String,
    #[serde(rename = "N")]
    pub commission_asset: Option<String>,
}

/// Raw kline from REST API (12 fields — all required by serde)
#[allow(dead_code)]
#[derive(Deserialize)]
pub struct RawKline(
    pub u64,    // 0  open_time
    pub String, // 1  open   ← needed for WickProfile
    pub String, // 2  high
    pub String, // 3  low
    pub String, // 4  close
    pub String, // 5  volume
    pub u64,    // 6  close_time
    pub String, // 7  quote_vol
    pub u64,    // 8  num_trades
    pub String, // 9  taker_buy_base
    pub String, // 10 taker_buy_quote
    pub String, // 11 ignore
);

// ── Core enums ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Trend  { Up, Down, Neutral }

#[derive(Debug, Clone, PartialEq)]
pub enum Regime { Low, Medium, High }

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Side   { Buy, Sell }

impl Side {
    pub fn binance_str(&self) -> &'static str {
        match self { Side::Buy => "BUY", Side::Sell => "SELL" }
    }
}

/// [FIX-3] EMA 9/21 trend signal — drives position size + grid spacing
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum TrendSignal {
    Bullish,
    Caution,
    Bearish,
}

impl TrendSignal {
    pub fn position_mult(&self) -> f64 {
        match self { Self::Bullish => 1.20, Self::Caution => 0.85, Self::Bearish => 0.70 }
    }
    pub fn spacing_mult(&self) -> f64 {
        match self { Self::Bullish => 0.90, Self::Caution => 1.00, Self::Bearish => 1.35 }
    }
    pub fn label(&self) -> &'static str {
        match self { Self::Bullish => "🟢 BULLISH", Self::Caution => "🟡 CAUTION", Self::Bearish => "🔴 BEARISH" }
    }
}

/// [FIX-2] TradingStatus — Instant doesn't impl PartialEq, use is_active()
#[derive(Debug, Clone)]
pub enum TradingStatus {
    Active,
    PausedUntil(std::time::Instant),
    SafetySwitchTripped { reason: String },
}

impl TradingStatus {
    pub fn is_active(&self)  -> bool { matches!(self, Self::Active) }
    pub fn is_tripped(&self) -> bool { matches!(self, Self::SafetySwitchTripped {..}) }
    pub fn label(&self) -> &str {
        match self {
            Self::Active                    => "ACTIVE",
            Self::PausedUntil(_)            => "PAUSED",
            Self::SafetySwitchTripped {..}  => "TRIPPED",
        }
    }
}

// ── [FIX-1] Candle now includes `open` ─────────────────────

#[derive(Debug, Clone)]
pub struct Candle {
    pub open:  f64,
    pub high:  f64,
    pub low:   f64,
    pub close: f64,
}

impl Candle {
    /// Body size as fraction of total range (0=doji, 1=marubozu)
    pub fn body_ratio(&self) -> f64 {
        let body  = (self.close - self.open).abs();
        let range = self.high - self.low;
        if range < f64::EPSILON { return 1.0; }
        (body / range).clamp(0.0, 1.0)
    }

    pub fn wick_ratio(&self)       -> f64 { 1.0 - self.body_ratio() }
    pub fn upper_wick_pct(&self)   -> f64 {
        let range = self.high - self.low;
        if range < f64::EPSILON { return 0.0; }
        (self.high - self.close.max(self.open)) / range
    }
    pub fn lower_wick_pct(&self)   -> f64 {
        let range = self.high - self.low;
        if range < f64::EPSILON { return 0.0; }
        (self.close.min(self.open) - self.low) / range
    }
}

/// [FIX-4] Wick analysis for variable slippage
#[derive(Debug, Clone, Default)]
pub struct WickProfile {
    pub avg_wick_ratio:  f64,
    pub long_wick_count: usize,  // candles where wick > 60% of range
}

impl WickProfile {
    pub fn from_candles(candles: &[Candle]) -> Self {
        if candles.is_empty() { return Self::default(); }
        let n   = candles.len() as f64;
        let avg = candles.iter().map(|c| c.wick_ratio()).sum::<f64>() / n;
        let cnt = candles.iter().filter(|c| c.wick_ratio() > 0.60).count();
        Self { avg_wick_ratio: avg, long_wick_count: cnt }
    }

    /// Returns multiplier ≥ 1.0 applied on top of base slippage
    pub fn slippage_mult(&self) -> f64 {
        let wick_boost = (self.avg_wick_ratio * 3.0).clamp(0.0, 2.0);
        let long_pct   = self.long_wick_count as f64 / 20.0_f64.max(1.0);
        (1.0 + wick_boost + long_pct * 1.5).clamp(1.0, 5.0)
    }
}

// ── Market state ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MarketState {
    pub price:        f64,
    pub atr:          f64,
    pub rsi:          f64,
    pub ema9:         f64,
    pub ema20:        f64,
    pub ema21:        f64,
    pub ema50:        f64,
    pub trend:        Trend,
    pub trend_signal: TrendSignal,
    pub regime:       Regime,
    pub spread_bps:   f64,
    pub wick_profile: WickProfile,
}

// ── Grid ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GridLevel {
    pub price:      f64,
    pub side:       Side,
    pub profit_pct: f64,
    pub distance:   f64,
    pub atr_dist:   f64,
}

// ── Trades ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    pub id:           String,
    pub time:         String,
    pub side:         Side,
    pub symbol:       String,
    pub price:        f64,
    pub exec_price:   f64,
    pub qty:          f64,
    pub value_usdt:   f64,
    pub slippage:     f64,
    pub slip_mult:    f64,
    pub trend_signal: String,
    pub binance_oid:  Option<u64>,
}

// ── Copy-trade signal (broadcast to followers) ───────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeSignal {
    pub master_id:   String,
    pub symbol:      String,
    pub side:        String,       // "BUY" / "SELL"
    pub price:       f64,
    pub qty_pct:     f64,          // % of follower's capital to use
    pub timestamp:   String,
    pub signal_hash: String,       // HMAC of signal for authenticity
}

// ── Follower record ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Follower {
    pub id:          String,
    pub webhook_url: String,
    pub active:      bool,
    pub paid_until:  String,       // ISO date
    pub total_trades: u64,
    pub registered:  String,
}

// ── Auto-optimizer result ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct OptResult {
    pub profit_target_pct: f64,
    pub final_value:       f64,
    pub net_pnl_pct:       f64,
    pub total_trades:      usize,
    pub max_drawdown_pct:  f64,
    pub sharpe_ratio:      f64,
    pub calmar_ratio:      f64,
}

// ── Helpers ──────────────────────────────────────────────────

pub fn regime_bucket(atr: f64, price: f64) -> u8 {
    let r = atr / price;
    if r < 0.008 { 0 } else if r < 0.020 { 1 } else { 2 }
}
