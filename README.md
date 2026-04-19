# SPD Grid Bot

A production-grade adaptive grid trading bot written in Rust, built for the Binance exchange.

> **Paper trading only** — no real funds are used unless you configure live API keys.

---

## What it does

- Connects to Binance via WebSocket for real-time prices (aggTrade stream)
- Fetches hourly candles via REST API to compute technical indicators
- Builds a dynamic grid of buy/sell levels based on ATR, EMA, and RSI
- Adapts grid spacing and position size to current market regime automatically
- Includes a copy-trade HTTP server so followers can mirror signals

---

## Features

| Feature | Detail |
|---|---|
| **Adaptive Grid** | ATR-based spacing — works on BNB, BTC, ETH without manual tuning |
| **Trend Filter** | EMA 9/21 crossover → Bullish / Caution / Bearish mode |
| **Variable Slippage** | Wick-severity model — more realistic than fixed slippage |
| **Risk Management** | Stop Loss, Max Drawdown, Safety Switch, Take Profit |
| **Copy-Trade Server** | HTTP endpoints for followers to receive signals |
| **Hot Config** | Edit `config.toml` without restarting |
| **Logging** | Daily rotating log files + colored terminal output |
| **Paper Mode** | Full simulation with no real money |

---

## Requirements

- Rust 1.75 or later
- Linux (tested on Ubuntu 22.04 and Arch)
- Internet connection

---

## Installation

```bash
git clone https://github.com/YOUR_USERNAME/spd-grid-bot
cd spd-grid-bot
cargo build --release
```

---

## Configuration

Edit `config.toml` before running:

```toml
symbol        = "BNBUSDT"
total_capital = 20.0
grid_levels   = 8

max_loss_pct      = 5.0
max_drawdown_pct  = 8.0
profit_target_pct = 15.0
```

Full reference is inside `config.toml` — every field is documented.

---

## Running

**Paper mode** (no API keys needed):

```bash
cargo run --release
```

**Live mode:**

```bash
export BINANCE_API_KEY=your_key
export BINANCE_API_SECRET=your_secret
cargo run --release
```

---

## Copy-Trade Server

When `copy_trade = true` in config, the bot exposes:

```
GET  /health      — liveness check
GET  /stats       — public performance stats
POST /register    — follower registration
POST /verify-fee  — admin: mark payment received
```

Default port: `8080`

---

## Sample Output

```
══════════ Cycle #1 tick=1 ══════════
📊 P:630.44 ATR:3.127 RSI:38.1 EMA9:631.75 EMA21:634.08 🔴 BEARISH
─────────────────────────────────────
  633.61  SELL   0.50%   0.200%  1.62×
  632.34  SELL   0.30%   0.200%  1.22×
  627.28   BUY   0.50%   0.202%  0.41×
  626.01   BUY   0.70%   0.202%  0.81×
─────────────────────────────────────
🧭 BEARISH — pos×0.70  spacing×1.35
📡 WS Latency: avg=117ms  σ=0.0ms
```

---

## Architecture

```
main.rs       — entry point, live/backtest mode selection
config.rs     — hot-reloadable configuration
analyzer.rs   — Binance REST + ATR / RSI / EMA indicators
engine.rs     — grid builder, risk manager, paper trader, optimizer
network.rs    — WebSocket price listener, rate limiter
models.rs     — all shared types and structs
```

---

## Indicators

All indicators use proper Wilder's smoothing (RMA), not simple moving averages:

- **ATR** — Wilder's RMA, outlier-capped per candle
- **RSI** — Wilder's smoothing (not SMA approximation)
- **EMA** — SMA seed + exponential, alpha = 2/(n+1)

---

## Disclaimer

This software is for educational and research purposes.
Trading involves significant risk of loss.
The author is not responsible for any financial losses.

---

## License

MIT
