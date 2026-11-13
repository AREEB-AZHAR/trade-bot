use std::{env, fmt, str::FromStr, time::Duration};

use rust_decimal::Decimal;
use thiserror::Error;

/// The only supported execution contexts. Shared core logic must not branch on
/// exchange implementation details; adapters are selected by this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMode {
    Backtest,
    Paper,
    Live,
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Backtest => "backtest",
            Self::Paper => "paper",
            Self::Live => "live",
        };
        formatter.write_str(value)
    }
}

impl FromStr for ExecutionMode {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "backtest" => Ok(Self::Backtest),
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            _ => Err(ConfigError::InvalidMode(value.to_owned())),
        }
    }
}

/// How every order in a single batch receives its position notional.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionSizingMode {
    FixedUsd,
    AccountEquityPercent,
}

impl fmt::Display for PositionSizingMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::FixedUsd => "fixed_usd",
            Self::AccountEquityPercent => "account_equity_percent",
        };
        formatter.write_str(value)
    }
}

impl FromStr for PositionSizingMode {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fixed_usd" => Ok(Self::FixedUsd),
            "account_equity_percent" => Ok(Self::AccountEquityPercent),
            _ => Err(ConfigError::InvalidPositionSizingMode(value.to_owned())),
        }
    }
}

/// Immutable sizing policy attached to one order batch.
///
/// Percentage sizing is resolved once against the account-equity snapshot at
/// batch start, never repeatedly while orders are being placed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PositionSizing {
    pub mode: PositionSizingMode,
    pub value: Decimal,
}

impl PositionSizing {
    /// Creates a validated sizing policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the size is not positive or when an account-equity
    /// percentage is greater than 100.
    pub fn new(mode: PositionSizingMode, value: Decimal) -> Result<Self, ConfigError> {
        if value <= Decimal::ZERO {
            return Err(ConfigError::InvalidPositionSizingValue {
                mode,
                value: value.to_string(),
            });
        }
        if mode == PositionSizingMode::AccountEquityPercent && value > Decimal::ONE_HUNDRED {
            return Err(ConfigError::InvalidPositionSizingValue {
                mode,
                value: value.to_string(),
            });
        }

        Ok(Self { mode, value })
    }

    /// Resolves the USDT notional for a batch from a consistent equity snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if percentage sizing is selected without a positive
    /// account-equity snapshot.
    pub fn resolve_notional(self, account_equity: Decimal) -> Result<Decimal, ConfigError> {
        match self.mode {
            PositionSizingMode::FixedUsd => Ok(self.value),
            PositionSizingMode::AccountEquityPercent if account_equity > Decimal::ZERO => {
                Ok(account_equity * self.value / Decimal::ONE_HUNDRED)
            }
            PositionSizingMode::AccountEquityPercent => Err(ConfigError::InvalidAccountEquity(
                account_equity.to_string(),
            )),
        }
    }
}

/// Non-secret configuration used by every runtime mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BotConfig {
    pub mode: ExecutionMode,
    pub symbols: Vec<String>,
    pub stale_data_threshold: Duration,
    pub taker_fee_bps: u32,
    pub position_sizing: PositionSizing,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("MODE must be set to backtest, paper, or live")]
    MissingMode,
    #[error("unsupported MODE value {0:?}; use backtest, paper, or live")]
    InvalidMode(String),
    #[error("live mode requires I_UNDERSTAND_THIS_IS_LIVE=true")]
    LiveModeNotConfirmed,
    #[error("{name} must be a positive integer, got {value:?}")]
    InvalidPositiveInteger { name: &'static str, value: String },
    #[error("SYMBOLS must contain at least one non-empty symbol")]
    EmptySymbols,
    #[error("unsupported position sizing mode {0:?}; use fixed_usd or account_equity_percent")]
    InvalidPositionSizingMode(String),
    #[error("POSITION_SIZE_VALUE must be a valid decimal, got {0:?}")]
    InvalidDecimal(String),
    #[error(
        "position sizing for {mode} must be positive and percentage sizing cannot exceed 100, got {value}"
    )]
    InvalidPositionSizingValue {
        mode: PositionSizingMode,
        value: String,
    },
    #[error("account equity must be positive for percentage sizing, got {0}")]
    InvalidAccountEquity(String),
}

impl BotConfig {
    /// Loads `.env` when present, then reads configuration from the process
    /// environment. Environment variables take precedence over `.env` values.
    ///
    /// # Errors
    ///
    /// Returns an error for missing or invalid values, including an unconfirmed
    /// request to run in live mode.
    pub fn from_env() -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();
        let mode = env::var("MODE").ok();
        let live_confirmation = env::var("I_UNDERSTAND_THIS_IS_LIVE").ok();
        let symbols = env::var("SYMBOLS").ok();
        let stale_data_threshold_ms = env::var("STALE_DATA_THRESHOLD_MS").ok();
        let taker_fee_bps = env::var("TAKER_FEE_BPS").ok();
        let position_size_mode = env::var("POSITION_SIZE_MODE").ok();
        let position_size_value = env::var("POSITION_SIZE_VALUE").ok();

        Self::from_values(
            mode.as_deref(),
            live_confirmation.as_deref(),
            symbols.as_deref(),
            stale_data_threshold_ms.as_deref(),
            taker_fee_bps.as_deref(),
            position_size_mode.as_deref(),
            position_size_value.as_deref(),
        )
    }

    /// Replaces the per-batch sizing policy with an explicit one-run override.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is not a valid, safe size for `mode`.
    pub fn with_position_sizing(
        mut self,
        mode: PositionSizingMode,
        value: &str,
    ) -> Result<Self, ConfigError> {
        self.position_sizing = PositionSizing::new(mode, parse_decimal(value)?)?;
        Ok(self)
    }

    fn from_values(
        mode: Option<&str>,
        live_confirmation: Option<&str>,
        symbols: Option<&str>,
        stale_data_threshold_ms: Option<&str>,
        taker_fee_bps: Option<&str>,
        position_size_mode: Option<&str>,
        position_size_value: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let mode = mode.ok_or(ConfigError::MissingMode)?.parse()?;

        if mode == ExecutionMode::Live && live_confirmation != Some("true") {
            return Err(ConfigError::LiveModeNotConfirmed);
        }

        let symbols = symbols
            .unwrap_or("BTC_USDT")
            .split(',')
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if symbols.is_empty() {
            return Err(ConfigError::EmptySymbols);
        }

        let stale_data_threshold = Duration::from_millis(u64::from(parse_positive_u32(
            "STALE_DATA_THRESHOLD_MS",
            stale_data_threshold_ms.unwrap_or("2000"),
        )?));
        let taker_fee_bps = parse_positive_u32("TAKER_FEE_BPS", taker_fee_bps.unwrap_or("5"))?;
        let position_sizing = PositionSizing::new(
            position_size_mode.unwrap_or("fixed_usd").parse()?,
            parse_decimal(position_size_value.unwrap_or("10"))?,
        )?;

        Ok(Self {
            mode,
            symbols,
            stale_data_threshold,
            taker_fee_bps,
            position_sizing,
        })
    }
}

fn parse_decimal(value: &str) -> Result<Decimal, ConfigError> {
    value
        .parse()
        .map_err(|_| ConfigError::InvalidDecimal(value.to_owned()))
}

fn parse_positive_u32(name: &'static str, value: &str) -> Result<u32, ConfigError> {
    value
        .parse::<u32>()
        .ok()
        .filter(|parsed| *parsed > 0)
        .ok_or_else(|| ConfigError::InvalidPositiveInteger {
            name,
            value: value.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::{BotConfig, ConfigError, ExecutionMode, PositionSizing, PositionSizingMode};
    use rust_decimal::Decimal;
    use std::{str::FromStr, time::Duration};

    #[test]
    fn backtest_accepts_defaults() {
        let config = BotConfig::from_values(Some("backtest"), None, None, None, None, None, None)
            .expect("backtest configuration should be valid");

        assert_eq!(config.mode, ExecutionMode::Backtest);
        assert_eq!(config.symbols, ["BTC_USDT"]);
        assert_eq!(config.stale_data_threshold, Duration::from_secs(2));
        assert_eq!(config.taker_fee_bps, 5);
        assert_eq!(
            config.position_sizing,
            PositionSizing::new(PositionSizingMode::FixedUsd, Decimal::TEN)
                .expect("default fixed size is valid")
        );
    }

    #[test]
    fn live_mode_fails_closed_without_explicit_confirmation() {
        let error = BotConfig::from_values(Some("live"), None, None, None, None, None, None)
            .expect_err("live mode must be explicitly confirmed");

        assert_eq!(error, ConfigError::LiveModeNotConfirmed);
    }

    #[test]
    fn live_mode_requires_exact_confirmation_value() {
        let config = BotConfig::from_values(
            Some("live"),
            Some("true"),
            Some("BTC_USDT, ETH_USDT"),
            Some("1500"),
            Some("10"),
            Some("account_equity_percent"),
            Some("2.5"),
        )
        .expect("confirmed live configuration should parse");

        assert_eq!(config.mode, ExecutionMode::Live);
        assert_eq!(config.symbols, ["BTC_USDT", "ETH_USDT"]);
        assert_eq!(config.stale_data_threshold, Duration::from_millis(1500));
        assert_eq!(config.taker_fee_bps, 10);
        assert_eq!(
            config.position_sizing.mode,
            PositionSizingMode::AccountEquityPercent
        );
        assert_eq!(config.position_sizing.value, Decimal::new(25, 1));
    }

    #[test]
    fn percentage_sizing_resolves_against_immutable_equity_snapshot() {
        let sizing = PositionSizing::new(
            PositionSizingMode::AccountEquityPercent,
            Decimal::from_str("2.5").expect("decimal should parse"),
        )
        .expect("percentage should be valid");

        assert_eq!(
            sizing
                .resolve_notional(Decimal::from(1_200_u32))
                .expect("positive equity should resolve"),
            Decimal::from(30_u32)
        );
    }

    #[test]
    fn percentage_sizing_cannot_exceed_full_account_equity() {
        let error = PositionSizing::new(
            PositionSizingMode::AccountEquityPercent,
            Decimal::from(101_u32),
        )
        .expect_err("percentage above 100 must fail");

        assert!(matches!(
            error,
            ConfigError::InvalidPositionSizingValue { .. }
        ));
    }
}
