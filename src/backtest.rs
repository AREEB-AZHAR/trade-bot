//! Offline replay harnesses that use historical timestamps rather than wall time.
//!
//! This module intentionally contains no strategy. `PlaceholderBacktest` exists
//! only to prove the path from a normalized historical tick to an open position,
//! then through the shared risk evaluator to a deterministic exit decision.

use rust_decimal::Decimal;
use thiserror::Error;

use crate::{
    domain::{MarketTick, OpenPosition, Side},
    risk::{RiskError, RiskExit, RiskManager},
};

/// Events emitted while proving the minimum backtest pipeline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaceholderBacktestEvent {
    /// The first tick opened a simulated long position.
    Entered { position: OpenPosition },
    /// A later tick was evaluated but did not trigger an exit.
    Holding,
    /// The risk layer closed the simulated position.
    Exited { exit: RiskExit },
    /// The single placeholder position was already closed.
    Complete,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BacktestError {
    #[error("placeholder entry notional must be positive, got {0}")]
    InvalidEntryNotional(Decimal),
    #[error("cannot open a placeholder position at non-positive price {0}")]
    InvalidEntryPrice(Decimal),
    #[error(transparent)]
    Risk(#[from] RiskError),
}

/// Minimal historical harness with exactly one simulated position.
///
/// It opens on the first tick, has no strategy-driven exit, and therefore only
/// closes when the shared risk layer returns an exit decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaceholderBacktest {
    risk_manager: RiskManager,
    entry_notional: Decimal,
    state: PlaceholderState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PlaceholderState {
    AwaitingFirstTick,
    Open(OpenPosition),
    Closed,
}

impl PlaceholderBacktest {
    /// Creates a one-position harness using the shared risk evaluator.
    ///
    /// # Errors
    ///
    /// Returns an error when the configured entry notional is not positive.
    pub fn new(risk_manager: RiskManager, entry_notional: Decimal) -> Result<Self, BacktestError> {
        if entry_notional <= Decimal::ZERO {
            return Err(BacktestError::InvalidEntryNotional(entry_notional));
        }

        Ok(Self {
            risk_manager,
            entry_notional,
            state: PlaceholderState::AwaitingFirstTick,
        })
    }

    /// Replays one historical tick through the placeholder pipeline.
    ///
    /// The first tick is the simulated first fill. Every subsequent tick calls
    /// [`RiskManager::evaluate`] with exchange timestamps, never wall-clock time.
    ///
    /// # Errors
    ///
    /// Returns an error when the first tick has an invalid price or when risk
    /// evaluation rejects malformed financial data.
    pub fn process_tick(
        &mut self,
        tick: &MarketTick,
    ) -> Result<PlaceholderBacktestEvent, BacktestError> {
        match &self.state {
            PlaceholderState::AwaitingFirstTick => self.open_on_first_tick(tick),
            PlaceholderState::Open(position) => {
                let position = position.clone();
                match self.risk_manager.evaluate(
                    &position,
                    tick.price,
                    tick.exchange_timestamp_ms,
                )? {
                    Some(exit) => {
                        self.state = PlaceholderState::Closed;
                        Ok(PlaceholderBacktestEvent::Exited { exit })
                    }
                    None => Ok(PlaceholderBacktestEvent::Holding),
                }
            }
            PlaceholderState::Closed => Ok(PlaceholderBacktestEvent::Complete),
        }
    }

    fn open_on_first_tick(
        &mut self,
        tick: &MarketTick,
    ) -> Result<PlaceholderBacktestEvent, BacktestError> {
        if tick.price <= Decimal::ZERO {
            return Err(BacktestError::InvalidEntryPrice(tick.price));
        }

        let position = OpenPosition {
            id: "placeholder-backtest-1".to_owned(),
            symbol: tick.symbol.clone(),
            side: Side::Long,
            entry_price: tick.price,
            filled_quantity: self.entry_notional / tick.price,
            first_fill_timestamp_ms: tick.exchange_timestamp_ms,
            entry_fee: self.entry_notional * self.risk_manager.policy().taker_fee_rate(),
        };
        self.state = PlaceholderState::Open(position.clone());
        Ok(PlaceholderBacktestEvent::Entered { position })
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::{PlaceholderBacktest, PlaceholderBacktestEvent};
    use crate::{domain::MarketTick, risk::RiskExitReason, risk::RiskManager};

    fn tick(timestamp_ms: i64, price: u32) -> MarketTick {
        MarketTick {
            symbol: "BTC_USDT".to_owned(),
            price: Decimal::from(price),
            exchange_timestamp_ms: timestamp_ms,
        }
    }

    #[test]
    fn placeholder_opens_on_first_tick_and_exits_via_shared_loss_rule() {
        let mut harness = PlaceholderBacktest::new(RiskManager::new(0), Decimal::from(100_u32))
            .expect("valid notional should create harness");

        let entry = harness
            .process_tick(&tick(1_000, 100))
            .expect("first tick should open position");
        let PlaceholderBacktestEvent::Entered { position } = entry else {
            panic!("first tick must create a position");
        };
        assert_eq!(position.filled_quantity, Decimal::ONE);
        assert_eq!(position.first_fill_timestamp_ms, 1_000);

        assert_eq!(
            harness
                .process_tick(&tick(1_500, 95))
                .expect("second tick should evaluate risk"),
            PlaceholderBacktestEvent::Holding
        );
        let exit = harness
            .process_tick(&tick(2_000, 89))
            .expect("third tick should evaluate risk");
        let PlaceholderBacktestEvent::Exited { exit } = exit else {
            panic!("loss limit must trigger an exit");
        };
        assert_eq!(exit.reason, RiskExitReason::MaxLoss);
    }

    #[test]
    fn placeholder_uses_historical_time_for_the_hard_exit() {
        let mut harness = PlaceholderBacktest::new(RiskManager::new(0), Decimal::from(100_u32))
            .expect("valid notional should create harness");

        harness
            .process_tick(&tick(20_000, 100))
            .expect("first tick should open position");
        assert_eq!(
            harness
                .process_tick(&tick(29_999, 100))
                .expect("pre-limit historical tick should evaluate"),
            PlaceholderBacktestEvent::Holding
        );
        let exit = harness
            .process_tick(&tick(30_000, 100))
            .expect("limit tick should evaluate");
        let PlaceholderBacktestEvent::Exited { exit } = exit else {
            panic!("ten historical seconds must trigger an exit");
        };
        assert_eq!(exit.reason, RiskExitReason::TimeLimit);
    }
}
