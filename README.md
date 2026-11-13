# HFT crypto trading bot

A Rust crypto perpetual-futures bot designed to progress through three separate
validation stages: local backtesting, paper trading, and only then live trading.

## Current foundation

- One shared `backtest` / `paper` / `live` mode model
- Fail-closed live-mode confirmation
- Environment-based configuration with a checked sample file
- Per-batch position sizing: fixed USDT or a percentage of account equity
- Structured `tracing` logging
- Shared domain types for the future market-data → strategy → risk → execution pipeline
- A streaming, backtest-only CSV tick reader with data-quality validation

The risk manager, exchange integration, and any live execution are intentionally
not implemented yet. The requirements leave one material risk-policy choice open:
whether the 10% loss and 20% profit thresholds are measured against position
notional or account equity. The owner has selected **position notional**; the
upcoming risk engine will treat that as fixed policy, never an adaptive setting.

## Run

```powershell
Copy-Item .env.example .env
cargo run
```

`MODE=live` is rejected unless `I_UNDERSTAND_THIS_IS_LIVE=true` is also present.
Even with that confirmation, there is no live execution backend in this foundation.

## Position size for an order batch

Set these values in `.env`, or override both for one run:

```powershell
cargo run -- --position-size-mode fixed_usd --position-size 100
cargo run -- --position-size-mode account_equity_percent --position-size 2.5
```

The fixed mode sends a 100 USDT notional instruction to every order in the
current batch. Percentage mode resolves the notional against the account-equity
snapshot taken when that batch begins; `2.5` means 2.5% of that snapshot. The
execution engine is not built yet, so no order can currently be sent.

## Inspect historical ticks

Historical input is deliberately local-only in backtest mode. The required CSV
schema is a header followed by exchange timestamps in milliseconds and decimal
prices:

```csv
timestamp_ms,price
1735689600000,95000.25
1735689601000,95001.00
```

Inspect and validate a one-symbol file without running a strategy:

```powershell
cargo run -- --inspect-historical ./data/BTC_USDT-2025-01-01.csv
```

An intentionally tiny valid file is included at
`examples/historical-ticks.csv` for a configuration smoke test.

The reader rejects malformed rows, non-positive prices, and timestamps that move
backwards. It emits the same decimal-based `MarketTick` type the risk layer uses.

## Placeholder risk backtest

Before a real strategy exists, the placeholder harness opens one simulated long
position on the first tick and holds it until the shared risk layer exits. It
uses historical timestamps, never an OS timer or wall clock:

```powershell
cargo run -- --placeholder-backtest examples/historical-ticks-loss.csv
```

This is deliberately a pipeline proof, not a profitability backtest and not a
strategy. The example exits through the `max_loss` rule.

## Next implementation sequence

1. Historical tick reader and clock abstraction for backtest mode
2. Isolated, exhaustively tested risk manager once the risk basis is chosen
3. Position/PnL tracker and simulated execution with fee/slippage models
4. Placeholder strategy and end-to-end one-symbol backtest
5. Paper data/execution adapters, then exchange-specific integration
