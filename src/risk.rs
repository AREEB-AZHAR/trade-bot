//! The deliberately small, synchronous risk core.
//!
//! Every tick for an open position must be evaluated here before any strategy
//! recommendation reaches execution. The thresholds are fixed in code: a 10%
//! fee-inclusive loss, a 20% fee-inclusive profit, or 10 seconds from first fill.

use std::{
    fmt,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use rust_decimal::Decimal;
use thiserror::Error;

use crate::domain::{OpenPosition, Side};

const MAX_POSITION_AGE: Duration = Duration::from_secs(10);

fn max_loss_ratio() -> Decimal {
    Decimal::new(10, 2)
}

fn max_profit_ratio() -> Decimal {
    Decimal::new(20, 2)
}

/// The risk policy has one intentionally non-configurable threshold basis.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiskThresholdBasis {
    PositionNotional,
}

impl fmt::Display for RiskThresholdBasis {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("position_notional")
    }
}

/// Hard risk thresholds plus the expected market-exit fee used in the `PnL` check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiskPolicy {
    pub basis: RiskThresholdBasis,
    pub taker_fee_bps: u32,
}

impl RiskPolicy {
    #[must_use]
    pub const fn new(taker_fee_bps: u32) -> Self {
        Self {
            basis: RiskThresholdBasis::PositionNotional,
            taker_fee_bps,
        }
    }

    #[must_use]
    pub fn max_loss_ratio(&self) -> Decimal {
        max_loss_ratio()
    }

    #[must_use]
    pub fn max_profit_ratio(&self) -> Decimal {
        max_profit_ratio()
    }

    #[must_use]
    pub const fn max_position_age(&self) -> Duration {
        MAX_POSITION_AGE
    }

    #[must_use]
    pub fn taker_fee_rate(&self) -> Decimal {
        Decimal::from(self.taker_fee_bps) / Decimal::from(10_000_u32)
    }
}

/// A reason for a risk-mandated market exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RiskExitReason {
    MaxLoss,
    MaxProfit,
    TimeLimit,
}

impl fmt::Display for RiskExitReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::MaxLoss => "max_loss",
            Self::MaxProfit => "max_profit",
            Self::TimeLimit => "time_limit",
        };
        formatter.write_str(value)
    }
}

/// The complete state captured when a non-negotiable exit is triggered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RiskExit {
    pub reason: RiskExitReason,
    pub position_id: String,
    pub position_notional: Decimal,
    pub mark_price: Decimal,
    pub net_unrealized_pnl: Decimal,
    /// Fee-inclusive `PnL` divided by entry position notional, e.g. `0.10` for 10%.
    pub net_unrealized_pnl_ratio: Decimal,
    pub position_age: Duration,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RiskError {
    #[error("position {position_id:?} has non-positive entry notional")]
    InvalidPositionNotional { position_id: String },
    #[error("mark price must be positive, got {0}")]
    InvalidMarkPrice(Decimal),
}

/// Stateless evaluator used identically by backtest, paper, and live modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RiskManager {
    policy: RiskPolicy,
}

impl RiskManager {
    #[must_use]
    pub const fn new(taker_fee_bps: u32) -> Self {
        Self {
            policy: RiskPolicy::new(taker_fee_bps),
        }
    }

    #[must_use]
    pub const fn policy(&self) -> RiskPolicy {
        self.policy
    }

    /// Evaluates fixed risk rules for one price tick.
    ///
    /// `PnL` rules take precedence over the time limit if a historical tick first
    /// arrives after 10 seconds with a `PnL` threshold already breached. This keeps
    /// the result deterministic and conforms to the requirement that the timer
    /// applies only when neither `PnL` rule has fired.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-positive mark price or malformed position
    /// notional. Both are unsafe inputs for financial calculations.
    pub fn evaluate(
        &self,
        position: &OpenPosition,
        mark_price: Decimal,
        current_timestamp_ms: i64,
    ) -> Result<Option<RiskExit>, RiskError> {
        if mark_price <= Decimal::ZERO {
            return Err(RiskError::InvalidMarkPrice(mark_price));
        }

        let position_notional = position.entry_notional();
        if position_notional <= Decimal::ZERO {
            return Err(RiskError::InvalidPositionNotional {
                position_id: position.id.clone(),
            });
        }

        let gross_pnl = match position.side {
            Side::Long => (mark_price - position.entry_price) * position.filled_quantity,
            Side::Short => (position.entry_price - mark_price) * position.filled_quantity,
        };
        let estimated_exit_fee =
            mark_price * position.filled_quantity * self.policy.taker_fee_rate();
        let net_unrealized_pnl = gross_pnl - position.entry_fee - estimated_exit_fee;
        let net_unrealized_pnl_ratio = net_unrealized_pnl / position_notional;
        let elapsed_ms = current_timestamp_ms
            .saturating_sub(position.first_fill_timestamp_ms)
            .max(0);
        let position_age = Duration::from_millis(elapsed_ms.unsigned_abs());

        let reason = if net_unrealized_pnl_ratio <= -self.policy.max_loss_ratio() {
            Some(RiskExitReason::MaxLoss)
        } else if net_unrealized_pnl_ratio >= self.policy.max_profit_ratio() {
            Some(RiskExitReason::MaxProfit)
        } else if position_age >= self.policy.max_position_age() {
            Some(RiskExitReason::TimeLimit)
        } else {
            None
        };

        Ok(reason.map(|reason| RiskExit {
            reason,
            position_id: position.id.clone(),
            position_notional,
            mark_price,
            net_unrealized_pnl,
            net_unrealized_pnl_ratio,
            position_age,
        }))
    }
}

/// A message emitted by an OS timer thread for paper or live positions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeLimitTrigger {
    pub position_id: String,
    pub reason: RiskExitReason,
}

/// Cancels a dedicated time-limit thread when an earlier exit closes a position.
///
/// Backtests must use historical timestamps with [`RiskManager::evaluate`] and
/// must not instantiate this type.
#[derive(Debug)]
pub struct PositionTimer {
    cancellation_sender: Option<Sender<()>>,
}

impl PositionTimer {
    /// Starts a dedicated OS thread that fires after the fixed 10-second limit.
    #[must_use]
    pub fn start(position_id: String, trigger_sender: Sender<TimeLimitTrigger>) -> Self {
        Self::start_with_delay(position_id, MAX_POSITION_AGE, trigger_sender)
    }

    fn start_with_delay(
        position_id: String,
        delay: Duration,
        trigger_sender: Sender<TimeLimitTrigger>,
    ) -> Self {
        let (cancellation_sender, cancellation_receiver) = mpsc::channel();
        thread::spawn(move || {
            if cancellation_receiver.recv_timeout(delay).is_err() {
                let _ = trigger_sender.send(TimeLimitTrigger {
                    position_id,
                    reason: RiskExitReason::TimeLimit,
                });
            }
        });

        Self {
            cancellation_sender: Some(cancellation_sender),
        }
    }

    /// Prevents the time-limit trigger after an earlier position exit.
    pub fn cancel(&mut self) {
        if let Some(sender) = self.cancellation_sender.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for PositionTimer {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;
    use rust_decimal::Decimal;

    use super::{RiskExitReason, RiskManager};
    use crate::domain::{OpenPosition, Side};

    fn position_with_quantity(
        side: Side,
        entry_price: Decimal,
        filled_quantity: Decimal,
        first_fill_timestamp_ms: i64,
        entry_fee: Decimal,
    ) -> OpenPosition {
        OpenPosition {
            id: "position-1".to_owned(),
            symbol: "BTC_USDT".to_owned(),
            side,
            entry_price,
            filled_quantity,
            first_fill_timestamp_ms,
            entry_fee,
        }
    }

    fn position(side: Side, entry_price: Decimal, entry_fee: Decimal) -> OpenPosition {
        position_with_quantity(side, entry_price, Decimal::ONE, 1_000, entry_fee)
    }

    #[test]
    fn loss_limit_is_fee_inclusive_and_measured_against_entry_notional() {
        let manager = RiskManager::new(5);
        let open_position = position(Side::Long, Decimal::from(100_u32), Decimal::new(5, 2));

        let exit = manager
            .evaluate(&open_position, Decimal::from(89_u32), 1_001)
            .expect("valid tick should evaluate")
            .expect("fee-inclusive loss crosses 10 percent");

        assert_eq!(exit.reason, RiskExitReason::MaxLoss);
        assert_eq!(exit.position_notional, Decimal::from(100_u32));
        assert!(exit.net_unrealized_pnl_ratio <= Decimal::new(-10, 2));
    }

    #[test]
    fn profit_limit_is_fee_inclusive_and_measured_against_entry_notional() {
        let manager = RiskManager::new(5);
        let open_position = position(Side::Short, Decimal::from(100_u32), Decimal::ZERO);

        let exit = manager
            .evaluate(&open_position, Decimal::from(79_u32), 1_001)
            .expect("valid tick should evaluate")
            .expect("profit crosses 20 percent");

        assert_eq!(exit.reason, RiskExitReason::MaxProfit);
    }

    #[test]
    fn time_limit_exits_at_exactly_ten_seconds_from_first_fill() {
        let manager = RiskManager::new(0);
        let open_position = position(Side::Long, Decimal::from(100_u32), Decimal::ZERO);

        assert!(
            manager
                .evaluate(&open_position, Decimal::from(100_u32), 10_999)
                .expect("valid tick should evaluate")
                .is_none()
        );
        let exit = manager
            .evaluate(&open_position, Decimal::from(100_u32), 11_000)
            .expect("valid tick should evaluate")
            .expect("position must exit at ten seconds");
        assert_eq!(exit.reason, RiskExitReason::TimeLimit);
    }

    #[test]
    fn partial_fill_uses_filled_quantity_for_notional_and_pnl() {
        let manager = RiskManager::new(0);
        let open_position = position_with_quantity(
            Side::Long,
            Decimal::from(100_u32),
            Decimal::new(5, 1),
            10_000,
            Decimal::ZERO,
        );

        let exit = manager
            .evaluate(&open_position, Decimal::from(89_u32), 10_001)
            .expect("valid partial-fill tick should evaluate")
            .expect("filled quantity has crossed the loss limit");

        assert_eq!(exit.reason, RiskExitReason::MaxLoss);
        assert_eq!(exit.position_notional, Decimal::from(50_u32));
        assert_eq!(exit.net_unrealized_pnl, Decimal::new(-55, 1));
    }

    #[test]
    fn time_limit_is_anchored_to_first_fill_timestamp() {
        let manager = RiskManager::new(0);
        let open_position = position_with_quantity(
            Side::Long,
            Decimal::from(100_u32),
            Decimal::ONE,
            20_000,
            Decimal::ZERO,
        );

        assert!(
            manager
                .evaluate(&open_position, Decimal::from(100_u32), 29_999)
                .expect("valid tick should evaluate")
                .is_none()
        );
        assert_eq!(
            manager
                .evaluate(&open_position, Decimal::from(100_u32), 30_000)
                .expect("valid tick should evaluate")
                .expect("ten seconds from first fill must exit")
                .reason,
            RiskExitReason::TimeLimit
        );
    }

    proptest! {
        #[test]
        fn any_long_position_crossing_ten_percent_loss_exits_within_one_tick(
            entry in 10_i64..1_000_000_i64,
            loss_bps in 1_000_i64..5_001_i64,
        ) {
            let manager = RiskManager::new(0);
            let entry_price = Decimal::from(entry);
            let open_position = position(Side::Long, entry_price, Decimal::ZERO);
            let mark_price = entry_price
                * Decimal::from(10_000_i64 - loss_bps)
                / Decimal::from(10_000_u32);
            let exit = manager
                .evaluate(&open_position, mark_price, 1_001)
                .expect("generated values must be valid");

            prop_assert_eq!(exit.map(|trigger| trigger.reason), Some(RiskExitReason::MaxLoss));
        }
    }

    #[test]
    fn timer_can_be_cancelled_before_it_fires() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut timer = super::PositionTimer::start("position-1".to_owned(), sender);
        timer.cancel();

        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
    }

    #[test]
    fn dedicated_timer_thread_emits_hard_time_exit() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _timer = super::PositionTimer::start_with_delay(
            "position-1".to_owned(),
            Duration::from_millis(20),
            sender,
        );

        let trigger = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("dedicated timer thread should emit a trigger");
        assert_eq!(trigger.position_id, "position-1");
        assert_eq!(trigger.reason, RiskExitReason::TimeLimit);
    }
}
