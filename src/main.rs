use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use hft_bot::config::{BotConfig, PositionSizingMode};
use hft_bot::market_data::HistoricalCsvFeed;
use hft_bot::{
    backtest::{PlaceholderBacktest, PlaceholderBacktestEvent},
    risk::RiskManager,
};
use rust_decimal::Decimal;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(version, about = "Safety-first crypto perpetual-futures trading bot")]
struct Args {
    /// Validate configuration, initialize logging, and exit without running a pipeline.
    #[arg(long)]
    check_config: bool,
    /// Override position sizing for this run's order batch.
    #[arg(long, value_enum, requires = "position_size")]
    position_size_mode: Option<PositionSizeModeArg>,
    /// Fixed USDT notional or account-equity percent, based on the selected mode.
    #[arg(long, requires = "position_size_mode")]
    position_size: Option<String>,
    /// Validate and summarize a local historical CSV file in backtest mode.
    #[arg(long, value_name = "PATH")]
    inspect_historical: Option<PathBuf>,
    /// Run the one-position placeholder risk harness against a local CSV file.
    #[arg(long, value_name = "PATH", conflicts_with = "inspect_historical")]
    placeholder_backtest: Option<PathBuf>,
    /// Account equity snapshot in USDT, required when percentage sizing is selected.
    #[arg(long, value_name = "USDT")]
    account_equity: Option<Decimal>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PositionSizeModeArg {
    FixedUsd,
    AccountEquityPercent,
}

impl From<PositionSizeModeArg> for PositionSizingMode {
    fn from(value: PositionSizeModeArg) -> Self {
        match value {
            PositionSizeModeArg::FixedUsd => Self::FixedUsd,
            PositionSizeModeArg::AccountEquityPercent => Self::AccountEquityPercent,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    initialize_logging();
    let args = Args::parse();
    let mut config = BotConfig::from_env()?;
    if let (Some(mode), Some(value)) = (args.position_size_mode, args.position_size.as_deref()) {
        config = config.with_position_sizing(mode.into(), value)?;
    }

    info!(
        mode = %config.mode,
        symbols = ?config.symbols,
        stale_data_threshold_ms = config.stale_data_threshold.as_millis(),
        taker_fee_bps = config.taker_fee_bps,
        position_sizing_mode = %config.position_sizing.mode,
        position_sizing_value = %config.position_sizing.value,
        "configuration loaded"
    );

    if let Some(path) = args.inspect_historical {
        if config.mode != hft_bot::config::ExecutionMode::Backtest {
            bail!("--inspect-historical is only available when MODE=backtest");
        }
        let mut feed = HistoricalCsvFeed::open(&config.symbols[0], &path)
            .with_context(|| format!("failed to open historical data at {}", path.display()))?;
        let mut count = 0_usize;
        let mut first_timestamp_ms = None;
        let mut last_timestamp_ms = None;
        for tick in &mut feed {
            let tick =
                tick.with_context(|| format!("invalid historical data at {}", path.display()))?;
            count += 1;
            first_timestamp_ms.get_or_insert(tick.exchange_timestamp_ms);
            last_timestamp_ms = Some(tick.exchange_timestamp_ms);
        }
        info!(
            path = %path.display(),
            symbol = %config.symbols[0],
            tick_count = count,
            ?first_timestamp_ms,
            ?last_timestamp_ms,
            "historical data validated"
        );
        return Ok(());
    }

    if let Some(path) = args.placeholder_backtest {
        if config.mode != hft_bot::config::ExecutionMode::Backtest {
            bail!("--placeholder-backtest is only available when MODE=backtest");
        }
        let account_equity = args.account_equity.unwrap_or(Decimal::ZERO);
        let entry_notional = config
            .position_sizing
            .resolve_notional(account_equity)
            .context("could not resolve the configured position size")?;
        let feed = HistoricalCsvFeed::open(&config.symbols[0], &path)
            .with_context(|| format!("failed to open historical data at {}", path.display()))?;
        let mut harness =
            PlaceholderBacktest::new(RiskManager::new(config.taker_fee_bps), entry_notional)
                .context("could not start placeholder backtest")?;
        let mut tick_count = 0_usize;
        let mut exit_reason = None;
        for tick in feed {
            let event =
                harness
                    .process_tick(&tick.with_context(|| {
                        format!("invalid historical data at {}", path.display())
                    })?)
                    .context("placeholder backtest failed")?;
            tick_count += 1;
            if let PlaceholderBacktestEvent::Exited { exit } = event {
                exit_reason = Some(exit.reason);
                break;
            }
        }
        info!(
            path = %path.display(),
            symbol = %config.symbols[0],
            entry_notional = %entry_notional,
            tick_count,
            ?exit_reason,
            "placeholder backtest completed"
        );
        return Ok(());
    }

    if args.check_config {
        return Ok(());
    }

    info!("pipeline foundation is ready; no market-data or execution adapter is enabled");
    Ok(())
}

fn initialize_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("hft_bot=info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
