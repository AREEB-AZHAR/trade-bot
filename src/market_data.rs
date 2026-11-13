//! Market-data sources and validation shared by backtest and future live feeds.
//!
//! Backtest mode intentionally uses no network calls. The historical reader emits
//! normalized [`MarketTick`] values, so downstream strategy and risk code cannot
//! distinguish a historical tick from a future exchange-adapter tick.

use std::{
    fs::File,
    io::{self, BufRead, BufReader, Lines},
    path::Path,
    time::Duration,
};

use rust_decimal::Decimal;
use thiserror::Error;

use crate::domain::MarketTick;

const EXPECTED_CSV_HEADER: [&str; 2] = ["timestamp_ms", "price"];

/// Failures encountered while opening or validating historical market data.
#[derive(Debug, Error)]
pub enum HistoricalDataError {
    #[error("unable to open historical data file {path:?}: {source}")]
    Open {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("historical data is missing the required timestamp_ms,price header")]
    MissingHeader,
    #[error("invalid historical CSV header {found:?}; expected timestamp_ms,price")]
    InvalidHeader { found: String },
    #[error("could not read historical data at line {line}: {source}")]
    Read {
        line: usize,
        #[source]
        source: io::Error,
    },
    #[error("historical data line {line} must contain exactly timestamp_ms,price")]
    InvalidColumnCount { line: usize },
    #[error("historical data line {line} has invalid timestamp_ms value {value:?}")]
    InvalidTimestamp { line: usize, value: String },
    #[error("historical data line {line} has invalid decimal price {value:?}")]
    InvalidPrice { line: usize, value: String },
    #[error("historical data line {line} has non-positive price {price}")]
    NonPositivePrice { line: usize, price: Decimal },
    #[error(
        "historical data timestamp moved backwards at line {line}: {timestamp_ms} follows {previous_timestamp_ms}"
    )]
    TimestampMovedBackwards {
        line: usize,
        previous_timestamp_ms: i64,
        timestamp_ms: i64,
    },
}

/// Streaming local CSV feed for one configured symbol.
///
/// The expected CSV schema is `timestamp_ms,price`. Equal timestamps are allowed
/// because an exchange can publish multiple events in the same millisecond; only
/// backwards timestamps are rejected.
#[derive(Debug)]
pub struct HistoricalCsvFeed<R: BufRead> {
    symbol: String,
    lines: Lines<R>,
    current_line: usize,
    previous_timestamp_ms: Option<i64>,
}

impl HistoricalCsvFeed<BufReader<File>> {
    /// Opens a local CSV file for streaming replay.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or does not have the
    /// required CSV header.
    pub fn open(
        symbol: impl Into<String>,
        path: impl AsRef<Path>,
    ) -> Result<Self, HistoricalDataError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| HistoricalDataError::Open {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_reader(symbol, BufReader::new(file))
    }
}

impl<R: BufRead> HistoricalCsvFeed<R> {
    /// Creates a historical feed from any buffered reader.
    ///
    /// # Errors
    ///
    /// Returns an error if the source does not begin with the exact required
    /// `timestamp_ms,price` header.
    pub fn from_reader(symbol: impl Into<String>, reader: R) -> Result<Self, HistoricalDataError> {
        let mut lines = reader.lines();
        let header = lines.next().ok_or(HistoricalDataError::MissingHeader)?;
        let header = header.map_err(|source| HistoricalDataError::Read { line: 1, source })?;
        let columns = header.split(',').map(str::trim).collect::<Vec<_>>();
        if columns.as_slice() != EXPECTED_CSV_HEADER {
            return Err(HistoricalDataError::InvalidHeader { found: header });
        }

        Ok(Self {
            symbol: symbol.into(),
            lines,
            current_line: 1,
            previous_timestamp_ms: None,
        })
    }
}

impl<R: BufRead> Iterator for HistoricalCsvFeed<R> {
    type Item = Result<MarketTick, HistoricalDataError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let line = self.lines.next()?;
            self.current_line += 1;
            let line = match line {
                Ok(line) => line,
                Err(source) => {
                    return Some(Err(HistoricalDataError::Read {
                        line: self.current_line,
                        source,
                    }));
                }
            };
            if line.trim().is_empty() {
                continue;
            }

            let columns = line.split(',').map(str::trim).collect::<Vec<_>>();
            if columns.len() != EXPECTED_CSV_HEADER.len() {
                return Some(Err(HistoricalDataError::InvalidColumnCount {
                    line: self.current_line,
                }));
            }
            let Ok(timestamp_ms) = columns[0].parse::<i64>() else {
                return Some(Err(HistoricalDataError::InvalidTimestamp {
                    line: self.current_line,
                    value: columns[0].to_owned(),
                }));
            };
            let Ok(price) = columns[1].parse::<Decimal>() else {
                return Some(Err(HistoricalDataError::InvalidPrice {
                    line: self.current_line,
                    value: columns[1].to_owned(),
                }));
            };
            if price <= Decimal::ZERO {
                return Some(Err(HistoricalDataError::NonPositivePrice {
                    line: self.current_line,
                    price,
                }));
            }
            if let Some(previous_timestamp_ms) = self.previous_timestamp_ms
                && timestamp_ms < previous_timestamp_ms
            {
                return Some(Err(HistoricalDataError::TimestampMovedBackwards {
                    line: self.current_line,
                    previous_timestamp_ms,
                    timestamp_ms,
                }));
            }

            self.previous_timestamp_ms = Some(timestamp_ms);
            return Some(Ok(MarketTick {
                symbol: self.symbol.clone(),
                price,
                exchange_timestamp_ms: timestamp_ms,
            }));
        }
    }
}

/// Records the most recent tick and reports whether the symbol is safe for new
/// entries. Existing positions must still be risk-evaluated on every available
/// tick even when this says the symbol is stale.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StalenessTracker {
    threshold: Duration,
    last_tick_timestamp_ms: Option<i64>,
}

impl StalenessTracker {
    #[must_use]
    pub const fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            last_tick_timestamp_ms: None,
        }
    }

    pub fn record(&mut self, tick: &MarketTick) {
        self.last_tick_timestamp_ms = Some(tick.exchange_timestamp_ms);
    }

    #[must_use]
    pub fn is_stale_at(&self, current_timestamp_ms: i64) -> bool {
        let Some(last_tick_timestamp_ms) = self.last_tick_timestamp_ms else {
            return true;
        };
        let elapsed_ms = current_timestamp_ms
            .saturating_sub(last_tick_timestamp_ms)
            .max(0)
            .unsigned_abs();
        u128::from(elapsed_ms) >= self.threshold.as_millis()
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, time::Duration};

    use rust_decimal::Decimal;

    use super::{HistoricalCsvFeed, HistoricalDataError, StalenessTracker};

    #[test]
    fn reads_decimal_ticks_in_timestamp_order() {
        let input =
            Cursor::new(b"timestamp_ms,price\n1000,95000.25\n1000,95001.00\n2000,94999.50\n");
        let ticks = HistoricalCsvFeed::from_reader("BTC_USDT", input)
            .expect("valid header should parse")
            .collect::<Result<Vec<_>, _>>()
            .expect("valid rows should parse");

        assert_eq!(ticks.len(), 3);
        assert_eq!(ticks[0].symbol, "BTC_USDT");
        assert_eq!(ticks[0].price, Decimal::new(9_500_025, 2));
        assert_eq!(ticks[2].exchange_timestamp_ms, 2_000);
    }

    #[test]
    fn rejects_timestamp_that_moves_backwards() {
        let input = Cursor::new(b"timestamp_ms,price\n2000,100\n1999,99\n");
        let mut feed =
            HistoricalCsvFeed::from_reader("BTC_USDT", input).expect("valid header should parse");

        assert!(feed.next().expect("first row exists").is_ok());
        let error = feed
            .next()
            .expect("second row exists")
            .expect_err("backwards timestamps must fail");
        assert!(matches!(
            error,
            HistoricalDataError::TimestampMovedBackwards { .. }
        ));
    }

    #[test]
    fn rejects_non_positive_prices() {
        let input = Cursor::new(b"timestamp_ms,price\n1000,0\n");
        let error = HistoricalCsvFeed::from_reader("BTC_USDT", input)
            .expect("valid header should parse")
            .next()
            .expect("row exists")
            .expect_err("zero prices are invalid");

        assert!(matches!(
            error,
            HistoricalDataError::NonPositivePrice { .. }
        ));
    }

    #[test]
    fn stale_data_blocks_new_entries_at_threshold() {
        let mut tracker = StalenessTracker::new(Duration::from_secs(2));
        assert!(tracker.is_stale_at(1_000));

        let tick = super::MarketTick {
            symbol: "BTC_USDT".to_owned(),
            price: Decimal::from(100_u32),
            exchange_timestamp_ms: 1_000,
        };
        tracker.record(&tick);

        assert!(!tracker.is_stale_at(2_999));
        assert!(tracker.is_stale_at(3_000));
    }
}
