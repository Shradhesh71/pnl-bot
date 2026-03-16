# pnl-bot

A DEX/CEX arbitrage engine for Solana, written in Rust. Detects price discrepancies between Binance (CEX) and Orca/Raydium (DEX) on the SOL/USDC pair using real-time Yellowstone Geyser streams and Binance WebSocket feeds.

---

## Architecture

```
Binance bookTicker WS          Yellowstone Geyser (gRPC)
        |                               |
        v                               v
  binance_ws.rs                  pool_monitor.rs
  update_cex_quote()    -->    update_dex() [whirlpool/raydium]
        |                               |
        +-----------> PriceState <------+
                           |
                    spread_detector.rs
                    (50ms tick, 5 gates)
                           |
                     SpreadSignal
                           |
              +------------+------------+
              |                         |
        PaperEngine               Coordinator
        pnl_tracker.rs         (live: tokio::join!)
                               CEX leg + DEX leg concurrent
```

**Detection latency:** Binance WS ~10-50ms, Geyser slot ~400ms. The binding constraint is Solana slot time.

---

## Project Structure

```
src/
  bin/
    paper_trade_demo.rs   -- Paper trading with simulated CEX prices
    live_trade.rs         -- Live feed with real Binance WS (paper mode default)
  dex/
    whirlpool.rs          -- Orca Whirlpool account decoder
    raydium.rs            -- Raydium CLMM PoolState decoder
    pool_monitor.rs       -- Geyser gRPC subscriber with auto-reconnect
  strategy/
    price_state.rs        -- Thread-safe shared price store (lock-free)
    spread_detector.rs    -- 5-gate signal pipeline
  paper/
    paper_engine.rs       -- Trade simulation with fill modeling
    pnl_tracker.rs        -- Incremental P&L metrics (Welford Sharpe, drawdown)
  execution/
    binance_ws.rs         -- Binance bookTicker WebSocket
    binance_rest.rs       -- Binance market order execution (HMAC-SHA256)
    jupiter.rs            -- Jupiter v6 swap quote and execution
    coordinator.rs        -- Concurrent leg orchestration
```

---

## Running

### Paper mode (no credentials required)

```bash
GEYSER_ENDPOINT="https://solana-rpc.parafi.tech:10443" \
GEYSER_X_TOKEN="your-token" \
RUST_LOG=warn \
cargo run --bin live_trade
```

### Paper trade demo (simulated CEX prices)

```bash
GEYSER_ENDPOINT="https://solana-rpc.parafi.tech:10443" \
GEYSER_X_TOKEN="your-token" \
cargo run --bin paper_trade
```

### Live mode (real orders — requires 20 profitable paper trades first)

```bash
GEYSER_ENDPOINT="..." \
GEYSER_X_TOKEN="..." \
BINANCE_API_KEY="..." \
BINANCE_API_SECRET="..." \
WALLET_KEYPAIR_PATH="/path/to/keypair.json" \
LIVE=true \
LIVE_CONFIRMED=yes \
cargo run --bin live_trade
```

---

## Configuration

| Variable              | Default    | Description                              |
|-----------------------|------------|------------------------------------------|
| `GEYSER_ENDPOINT`     | required   | Yellowstone gRPC endpoint                |
| `GEYSER_X_TOKEN`      | required   | Geyser auth token                        |
| `INITIAL_BALANCE`     | 10000.0    | Starting USDC balance for paper trading  |
| `TRADE_SIZE`          | 1000.0     | Notional per trade in USDC               |
| `MIN_NET_SPREAD_PCT`  | 0.20       | Minimum net spread to trigger a signal   |
| `BINANCE_API_KEY`     | —          | Required for live CEX execution          |
| `BINANCE_API_SECRET`  | —          | Required for live CEX execution          |
| `WALLET_KEYPAIR_PATH` | —          | Path to Solana keypair JSON for DEX leg  |
| `LIVE`                | false      | Enable live order execution              |
| `LIVE_CONFIRMED`      | —          | Must be `yes` to proceed in live mode    |
| `RUST_LOG`            | warn       | Tracing filter                           |

---

## Signal Detection Gates

A spread signal must pass all five gates in order:

1. **Cooldown** — 500ms minimum between signals
2. **Staleness** — DEX price must be within 3 slots (~1.2s)
3. **Liquidity** — Pool must have at least $50k liquidity
4. **Spread** — Raw spread must exceed total fees (0.15% for SOL/USDC)
5. **Net profit** — Net after fees and slippage must exceed `MIN_NET_SPREAD_PCT`

Cross-DEX sanity check rejects any DEX price that deviates more than 3% from the other DEX — guards against partial Geyser account updates producing garbage decode values.

---

## Live Mode Safety Gates

Real order execution requires all of the following:

- `LIVE=true` and `LIVE_CONFIRMED=yes` set explicitly
- Minimum 50 completed paper trades on record
- Paper trading win rate above 60%
- Sufficient USDC balance confirmed via Binance REST before each trade
- Signal slot age within 2 slots at execution time

---

## Known Limitations

SOL/USDC on Orca and Raydium is among the most efficiently arbitraged pairs on Solana. Professional bots operating at sub-10ms latency close most spreads before a Geyser-based detector sees them. Observed signal frequency on this pair during normal market conditions is 0-5 per hour, concentrated during high-volatility events. Less liquid pairs (WIF, BONK, JTO) are more viable targets at Geyser latency.

---

## License

MIT