// ═══════════════════════════════════════════════════════════
//  network.rs — SPD Software Lab  v3.0 PRODUCTION
//  WebSocket price listener + Token Bucket rate limiter
//  Uses aggTrade stream (lower latency than kline)
// ═══════════════════════════════════════════════════════════

use crate::models::WsTrade;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{watch, Mutex};
use tokio::time::sleep;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

// ── Token Bucket Rate Limiter ────────────────────────────────

pub struct RateLimiter {
    tokens:         f64,
    max_tokens:     f64,
    refill_per_sec: f64,
    last_refill:    Instant,
}

impl RateLimiter {
    pub fn new(rps: u64) -> Self {
        Self {
            tokens:         rps as f64,
            max_tokens:     rps as f64,
            refill_per_sec: rps as f64,
            last_refill:    Instant::now(),
        }
    }

    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.max_tokens);
        self.last_refill = Instant::now();
    }

    pub async fn acquire(&mut self) {
        loop {
            self.refill();
            if self.tokens >= 1.0 { self.tokens -= 1.0; return; }
            let ms = ((1.0 - self.tokens) / self.refill_per_sec * 1000.0) as u64 + 1;
            sleep(Duration::from_millis(ms)).await;
        }
    }
}

// ── Price staleness guard ────────────────────────────────────

pub struct PriceUpdate {
    pub price:      f64,
    pub received:   Instant,
}

impl PriceUpdate {
    pub fn age_ms(&self) -> u64 {
        self.received.elapsed().as_millis() as u64
    }
    pub fn is_stale(&self, max_age_ms: u64) -> bool {
        self.age_ms() > max_age_ms
    }
}

// ── WebSocket price listener ──────────────────────────────────

pub async fn ws_price_listener(
    tx:           watch::Sender<Option<f64>>,
    latencies:    Arc<Mutex<VecDeque<u64>>>,
    symbol_lower: String,
    buf_size:     usize,
) {
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use futures_util::StreamExt;

    // aggTrade: lower latency, aggregates simultaneous trades
    let url     = format!("wss://stream.binance.com:9443/ws/{}@aggTrade", symbol_lower);
    let mut backoff = 1u64;

    loop {
        info!("  🔌 [WS] Connecting to {}@aggTrade...", symbol_lower);
        match connect_async(&url).await {
            Err(e) => {
                error!("  ❌ [WS] Connect failed: {} — retry {}s", e, backoff);
                sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
            Ok((mut stream, _)) => {
                info!("  ✅ [WS] Connected — receiving prices");
                backoff = 1;

                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Ok(trade) = serde_json::from_str::<WsTrade>(&text) {
                                if let Ok(price) = trade.price.parse::<f64>() {
                                    let now_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as u64;
                                    let latency = now_ms.saturating_sub(trade.trade_time);

                                    {
                                        let mut lat = latencies.lock().await;
                                        lat.push_back(latency);
                                        if lat.len() > buf_size { lat.pop_front(); }
                                    }

                                    let _ = tx.send(Some(price));
                                }
                            }
                        }
                        // tokio-tungstenite handles Pong automatically [F2]
                        Ok(Message::Ping(_))  => {}
                        Ok(Message::Close(_)) => {
                            warn!("  ⚠️  [WS] Server closed — reconnecting");
                            break;
                        }
                        Err(e) => {
                            error!("  ❌ [WS] Error: {} — backoff {}s", e, backoff);
                            sleep(Duration::from_secs(backoff)).await;
                            backoff = (backoff * 2).min(60);
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}
