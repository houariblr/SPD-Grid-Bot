// ═══════════════════════════════════════════════════════════
//  trader.rs — SPD Software Lab  v3.0 PRODUCTION
//  Real Binance order execution via signed REST API
//  HMAC-SHA256 signing per Binance spec
// ═══════════════════════════════════════════════════════════

use crate::models::{Side, TradeRecord, WsExecutionReport};
use chrono::Local;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const BINANCE_BASE: &str = "https://api.binance.com";
const BINANCE_WS:   &str = "wss://stream.binance.com:9443/ws";

// ── Binance response types ───────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct OrderResponse {
    pub symbol:              String,
    #[serde(rename = "orderId")]
    pub order_id:            u64,
    pub status:              String,
    #[serde(rename = "executedQty")]
    pub executed_qty:        String,
    #[serde(rename = "cummulativeQuoteQty")]
    pub cumulative_quote_qty: String,
    pub price:               String,
}

#[derive(Deserialize, Debug)]
pub struct AccountInfo {
    pub balances: Vec<Balance>,
}

#[derive(Deserialize, Debug)]
pub struct Balance {
    pub asset:  String,
    pub free:   String,
    pub locked: String,
}

#[derive(Deserialize, Debug)]
pub struct TickerPrice {
    pub symbol: String,
    pub price:  String,
}

#[derive(Deserialize, Debug)]
pub struct ListenKeyResponse {
    #[serde(rename = "listenKey")]
    pub listen_key: String,
}

// ── Signing ──────────────────────────────────────────────────

fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn sign_payload(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC key error");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

// ── Binance Trader ───────────────────────────────────────────

pub struct BinanceTrader {
    client:     reqwest::Client,
    api_key:    String,
    api_secret: String,
    symbol:     String,
    dry_run:    bool,   // safety: logs order but doesn't send
}

impl BinanceTrader {
    pub fn new(api_key: String, api_secret: String, symbol: String, dry_run: bool) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api_key, api_secret, symbol, dry_run,
        }
    }

    // ─────────────────────────────────────────────────────
    //  Place a LIMIT order (GTC)
    // ─────────────────────────────────────────────────────
    pub async fn place_limit_order(
        &self,
        side:     &Side,
        quantity: f64,
        price:    f64,
    ) -> Result<OrderResponse, String> {
        let ts   = timestamp_ms();
        let qty  = format!("{:.4}", quantity);
        let prc  = format!("{:.4}", price);

        let params = format!(
            "symbol={}&side={}&type=LIMIT&timeInForce=GTC&quantity={}&price={}&timestamp={}",
            self.symbol, side.binance_str(), qty, prc, ts
        );
        let sig = sign_payload(&self.api_secret, &params);
        let body = format!("{}&signature={}", params, sig);

        if self.dry_run {
            info!("  [DRY-RUN] LIMIT {} {} @ {} qty={}", side.binance_str(), self.symbol, prc, qty);
            return Ok(OrderResponse {
                symbol:              self.symbol.clone(),
                order_id:            0,
                status:              "DRY_RUN".into(),
                executed_qty:        qty,
                cumulative_quote_qty: "0".into(),
                price:               prc,
            });
        }

        let resp = self.client
            .post(format!("{}/api/v3/order", BINANCE_BASE))
            .header("X-MBX-APIKEY", &self.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("request error: {}", e))?;

        let status = resp.status();
        let text   = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            error!("[trader] Order failed: {} — {}", status, text);
            return Err(format!("HTTP {} — {}", status, text));
        }

        serde_json::from_str::<OrderResponse>(&text)
            .map_err(|e| format!("parse error: {} — body: {}", e, text))
    }

    // ─────────────────────────────────────────────────────
    //  Cancel an order
    // ─────────────────────────────────────────────────────
    pub async fn cancel_order(&self, order_id: u64) -> Result<(), String> {
        if self.dry_run { return Ok(()); }
        let ts     = timestamp_ms();
        let params = format!("symbol={}&orderId={}&timestamp={}", self.symbol, order_id, ts);
        let sig    = sign_payload(&self.api_secret, &params);
        let body   = format!("{}&signature={}", params, sig);

        let resp = self.client
            .delete(format!("{}/api/v3/order", BINANCE_BASE))
            .header("X-MBX-APIKEY", &self.api_key)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("cancel error: {}", e))?;

        if !resp.status().is_success() {
            warn!("[trader] Cancel failed for orderId={}", order_id);
        }
        Ok(())
    }

    // ─────────────────────────────────────────────────────
    //  Get current price
    // ─────────────────────────────────────────────────────
    pub async fn get_price(&self) -> Result<f64, String> {
        let resp = self.client
            .get(format!("{}/api/v3/ticker/price?symbol={}", BINANCE_BASE, self.symbol))
            .send().await
            .map_err(|e| format!("price error: {}", e))?;

        let t: TickerPrice = resp.json().await
            .map_err(|e| format!("parse: {}", e))?;

        t.price.parse::<f64>().map_err(|e| format!("f64: {}", e))
    }

    // ─────────────────────────────────────────────────────
    //  Get account balances
    // ─────────────────────────────────────────────────────
    pub async fn get_balances(&self) -> Result<(f64, f64), String> {
        let ts     = timestamp_ms();
        let params = format!("timestamp={}", ts);
        let sig    = sign_payload(&self.api_secret, &params);

        let resp = self.client
            .get(format!("{}/api/v3/account?{}&signature={}", BINANCE_BASE, params, sig))
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await
            .map_err(|e| format!("balance error: {}", e))?;

        let info: AccountInfo = resp.json().await
            .map_err(|e| format!("parse: {}", e))?;

        let usdt  = info.balances.iter()
            .find(|b| b.asset == "USDT")
            .and_then(|b| b.free.parse::<f64>().ok())
            .unwrap_or(0.0);

        // Extract base asset (e.g. "BNB" from "BNBUSDT")
        let base_asset = self.symbol.trim_end_matches("USDT")
                                    .trim_end_matches("BTC")
                                    .trim_end_matches("ETH");
        let base  = info.balances.iter()
            .find(|b| b.asset == base_asset)
            .and_then(|b| b.free.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok((usdt, base))
    }

    // ─────────────────────────────────────────────────────
    //  Create user data stream listen key
    // ─────────────────────────────────────────────────────
    pub async fn create_listen_key(&self) -> Result<String, String> {
        let resp = self.client
            .post(format!("{}/api/v3/userDataStream", BINANCE_BASE))
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await
            .map_err(|e| format!("listenKey error: {}", e))?;

        let lk: ListenKeyResponse = resp.json().await
            .map_err(|e| format!("parse: {}", e))?;

        Ok(lk.listen_key)
    }

    // ─────────────────────────────────────────────────────
    //  Keepalive listen key (call every 30 min)
    // ─────────────────────────────────────────────────────
    pub async fn keepalive_listen_key(&self, key: &str) {
        let _ = self.client
            .put(format!("{}/api/v3/userDataStream?listenKey={}", BINANCE_BASE, key))
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await;
    }

    // ─────────────────────────────────────────────────────
    //  Execute a real grid trade and return TradeRecord
    // ─────────────────────────────────────────────────────
    pub async fn execute_grid_trade(
        &self,
        side:        &Side,
        grid_price:  f64,
        quantity:    f64,
        symbol:      &str,
    ) -> Option<TradeRecord> {
        info!("  🔴 [LIVE] Placing {} {} qty={:.6} @ {:.4}",
            side.binance_str(), symbol, quantity, grid_price);

        match self.place_limit_order(side, quantity, grid_price).await {
            Err(e) => {
                error!("[trader] Order failed: {}", e);
                None
            }
            Ok(resp) => {
                let exec_price = resp.price.parse::<f64>().unwrap_or(grid_price);
                let exec_qty   = resp.executed_qty.parse::<f64>().unwrap_or(quantity);
                let value      = exec_qty * exec_price;

                info!("  ✅ [LIVE] {} orderId={} status={} execQty={:.6} val=${:.2}",
                    side.binance_str(), resp.order_id, resp.status, exec_qty, value);

                Some(TradeRecord {
                    id:          Uuid::new_v4().to_string(),
                    time:        Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    side:        side.clone(),
                    symbol:      symbol.to_string(),
                    price:       grid_price,
                    exec_price,
                    qty:         exec_qty,
                    value_usdt:  value,
                    slippage:    (exec_price - grid_price).abs() * exec_qty,
                    slip_mult:   1.0,
                    trend_signal: String::new(),
                    binance_oid: Some(resp.order_id),
                })
            }
        }
    }
}

// ─────────────────────────────────────────────────────────
//  User Data Stream — listens for FILLED events
//  Sends filled TradeRecords to a channel
// ─────────────────────────────────────────────────────────
pub async fn user_data_stream(
    listen_key: String,
    tx:         mpsc::Sender<WsExecutionReport>,
) {
    use tokio_tungstenite::{connect_async, tungstenite::Message};
    use futures_util::StreamExt;

    let url = format!("{}/{}", BINANCE_WS, listen_key);
    let mut backoff = 1u64;

    loop {
        info!("  🔌 [UserDataStream] connecting...");
        match connect_async(&url).await {
            Err(e) => {
                error!("[UserDataStream] connect error: {} — retry {}s", e, backoff);
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
                continue;
            }
            Ok((mut stream, _)) => {
                info!("  ✅ [UserDataStream] connected");
                backoff = 1;

                while let Some(msg) = stream.next().await {
                    if let Ok(Message::Text(text)) = msg {
                        if let Ok(report) = serde_json::from_str::<WsExecutionReport>(&text) {
                            if report.event_type == "executionReport"
                                && (report.status == "FILLED" || report.status == "PARTIALLY_FILLED")
                            {
                                let _ = tx.send(report).await;
                            }
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}
