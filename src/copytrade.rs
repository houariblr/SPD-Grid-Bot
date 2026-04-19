// ═══════════════════════════════════════════════════════════
//  copytrade.rs — SPD Software Lab  v3.0 PRODUCTION
//
//  Copy-trade server: master broadcasts signals to followers.
//  Followers pay subscription fee → master earns passive income.
//
//  Architecture:
//  ┌─────────────────────────────────────────────────────┐
//  │  Master Bot (this)                                   │
//  │  ┌────────────┐   trade signal   ┌───────────────┐  │
//  │  │ Grid Engine│ ────────────────►│ CopyBroadcast │  │
//  │  └────────────┘                  └──────┬────────┘  │
//  │                                         │            │
//  │  HTTP Server (axum)                     │ webhook    │
//  │  POST /register  ← follower signs up    ▼            │
//  │  GET  /stats     ← public performance   ┌──────────┐ │
//  │  POST /webhook   ← admin verify fee     │Follower 1│ │
//  └─────────────────────────────────────────└──────────┘ │
//                                             └──────────┘
// ═══════════════════════════════════════════════════════════

use crate::models::{Follower, TradeRecord, TradeSignal};
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use chrono::Local;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

// ── Shared state ─────────────────────────────────────────────

#[derive(Clone)]
pub struct CopyTradeState {
    pub followers:    Arc<RwLock<Vec<Follower>>>,
    pub signal_log:   Arc<RwLock<Vec<TradeSignal>>>,
    pub total_trades: Arc<RwLock<u64>>,
    pub master_id:    String,
    pub secret:       String,  // HMAC secret for signal signing
    pub fee_pct:      f64,
}

impl CopyTradeState {
    pub fn new(master_id: String, fee_pct: f64) -> Self {
        let secret = std::env::var("COPY_TRADE_SECRET")
            .unwrap_or_else(|_| Uuid::new_v4().to_string());
        Self {
            followers:    Arc::new(RwLock::new(Vec::new())),
            signal_log:   Arc::new(RwLock::new(Vec::new())),
            total_trades: Arc::new(RwLock::new(0)),
            master_id, secret, fee_pct,
        }
    }
}

// ── HTTP request/response types ──────────────────────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub webhook_url: String,
    pub paid_until:  String,   // ISO date "2025-12-31"
}

#[derive(Serialize)]
pub struct RegisterResponse {
    pub follower_id: String,
    pub message:     String,
    pub fee_pct:     f64,
}

#[derive(Serialize)]
pub struct StatsResponse {
    pub master_id:    String,
    pub total_trades: u64,
    pub followers:    usize,
    pub active_followers: usize,
    pub recent_signals: Vec<TradeSignal>,
    pub fee_pct:      f64,
}

#[derive(Deserialize)]
pub struct FeeVerifyRequest {
    pub follower_id: String,
    pub paid_until:  String,
    pub admin_token: String,
}

// ── Signal signing ───────────────────────────────────────────

fn sign_signal(secret: &str, symbol: &str, side: &str, price: f64, ts: &str) -> String {
    let payload = format!("{}{}{:.6}{}", symbol, side, price, ts);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())[..16].to_string()
}

// ── Broadcast engine ─────────────────────────────────────────

pub struct CopyBroadcast {
    state:  CopyTradeState,
    client: reqwest::Client,
}

impl CopyBroadcast {
    pub fn new(state: CopyTradeState) -> Self {
        Self {
            state,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Called after every real trade — broadcasts to all active followers
    pub async fn broadcast(&self, trade: &TradeRecord) {
        let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();

        let signal = TradeSignal {
            master_id:   self.state.master_id.clone(),
            symbol:      trade.symbol.clone(),
            side:        trade.side.binance_str().to_string(),
            price:       trade.exec_price,
            qty_pct:     0.09,  // followers use 9% of their capital (same as master default)
            timestamp:   now.clone(),
            signal_hash: sign_signal(
                &self.state.secret,
                &trade.symbol,
                trade.side.binance_str(),
                trade.exec_price,
                &now,
            ),
        };

        // Log signal
        {
            let mut log = self.state.signal_log.write().await;
            log.push(signal.clone());
            if log.len() > 1000 { log.drain(..500); }
        }

        *self.state.total_trades.write().await += 1;

        // Broadcast to active followers
        let followers = self.state.followers.read().await.clone();
        let today = Local::now().format("%Y-%m-%d").to_string();

        let mut active = 0;
        for f in followers.iter().filter(|f| f.active && f.paid_until >= today) {
            active += 1;
            let signal_clone = signal.clone();
            let url          = f.webhook_url.clone();
            let fid          = f.id.clone();
            let client       = self.client.clone();

            tokio::spawn(async move {
                match client.post(&url).json(&signal_clone).send().await {
                    Ok(r) if r.status().is_success() => {
                        info!("  📡 [copy] Signal → follower {} ✓", &fid[..8]);
                    }
                    Ok(r) => {
                        warn!("  📡 [copy] follower {} returned {}", &fid[..8], r.status());
                    }
                    Err(e) => {
                        error!("  📡 [copy] follower {} unreachable: {}", &fid[..8], e);
                    }
                }
            });
        }

        info!("  📡 [copy] Broadcasted to {}/{} active followers", active, followers.len());
    }
}

// ── Axum HTTP handlers ───────────────────────────────────────

/// POST /register — follower signs up
async fn handle_register(
    State(state): State<CopyTradeState>,
    Json(req):    Json<RegisterRequest>,
) -> impl IntoResponse {
    if req.webhook_url.is_empty() || !req.webhook_url.starts_with("http") {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "error": "Invalid webhook_url"
        }))).into_response();
    }

    let id = Uuid::new_v4().to_string();
    let follower = Follower {
        id:           id.clone(),
        webhook_url:  req.webhook_url.clone(),
        active:       true,
        paid_until:   req.paid_until.clone(),
        total_trades: 0,
        registered:   Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
    };

    state.followers.write().await.push(follower);

    info!("  👤 [copy] New follower registered: {} expires={}", &id[..8], req.paid_until);

    (StatusCode::CREATED, Json(RegisterResponse {
        follower_id: id,
        message: format!(
            "Registered! Master will broadcast signals to {}. Fee: {:.0}% of profits.",
            req.webhook_url, state.fee_pct
        ),
        fee_pct: state.fee_pct,
    })).into_response()
}

/// GET /stats — public performance stats
async fn handle_stats(State(state): State<CopyTradeState>) -> impl IntoResponse {
    let followers = state.followers.read().await;
    let today     = Local::now().format("%Y-%m-%d").to_string();
    let active    = followers.iter().filter(|f| f.active && f.paid_until >= today).count();

    let recent: Vec<TradeSignal> = state.signal_log.read().await
        .iter().rev().take(10).cloned().collect();

    Json(StatsResponse {
        master_id:       state.master_id.clone(),
        total_trades:    *state.total_trades.read().await,
        followers:       followers.len(),
        active_followers: active,
        recent_signals:  recent,
        fee_pct:         state.fee_pct,
    })
}

/// POST /verify-fee — admin endpoint to mark follower as paid
async fn handle_verify_fee(
    State(state): State<CopyTradeState>,
    Json(req):    Json<FeeVerifyRequest>,
) -> impl IntoResponse {
    // Simple admin token check
    let expected = std::env::var("ADMIN_TOKEN").unwrap_or_else(|_| "changeme".into());
    if req.admin_token != expected {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({
            "error": "Invalid admin token"
        }))).into_response();
    }

    let mut followers = state.followers.write().await;
    if let Some(f) = followers.iter_mut().find(|f| f.id == req.follower_id) {
        f.paid_until = req.paid_until.clone();
        f.active     = true;
        info!("  💰 [copy] Follower {} paid until {}", &req.follower_id[..8], req.paid_until);
        (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "follower_id": req.follower_id,
            "paid_until": req.paid_until
        }))).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({
            "error": "Follower not found"
        }))).into_response()
    }
}

/// GET /health
async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "SPD Grid Bot — OK")
}

// ── Start the copy-trade server ──────────────────────────────

pub async fn start_server(state: CopyTradeState, port: u16) {
    let app = Router::new()
        .route("/health",     get(handle_health))
        .route("/stats",      get(handle_stats))
        .route("/register",   post(handle_register))
        .route("/verify-fee", post(handle_verify_fee))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    info!("  🌐 [copy] Copy-trade server on http://{}", addr);
    info!("  🌐 [copy] Endpoints:");
    info!("           GET  /health      — liveness check");
    info!("           GET  /stats       — public performance");
    info!("           POST /register    — follower registration");
    info!("           POST /verify-fee  — admin: mark payment");

    let listener = tokio::net::TcpListener::bind(&addr).await
        .expect("Failed to bind copy-trade port");

    axum::serve(listener, app).await
        .expect("Copy-trade server crashed");
}
