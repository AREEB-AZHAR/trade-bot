use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Direction of a position or order. Prices and `PnL` always use `Decimal`; never
/// introduce binary floating-point values into financial calculations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    #[must_use]
    pub const fn closing_side(self) -> Self {
        match self {
            Self::Long => Self::Short,
            Self::Short => Self::Long,
        }
    }
}

/// A strategy recommendation. Signals are deliberately data-only: strategy code
/// cannot place an order or bypass the risk manager.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    Enter {
        side: Side,
        suggested_quantity: Decimal,
        confidence: Decimal,
    },
    Exit,
    Hold,
}

/// A normalized market price update. Exchange-specific adapters must convert
/// their payloads to this type before invoking strategy or risk logic.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MarketTick {
    pub symbol: String,
    pub price: Decimal,
    /// Exchange timestamp in Unix milliseconds, not local receive time.
    pub exchange_timestamp_ms: i64,
}

/// A position formed from actual fills. `first_fill_timestamp_ms` anchors all
/// exposure timing; order-submission time is deliberately not represented here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OpenPosition {
    pub id: String,
    pub symbol: String,
    pub side: Side,
    pub entry_price: Decimal,
    pub filled_quantity: Decimal,
    pub first_fill_timestamp_ms: i64,
    /// Actual fee paid for all entry fills, denominated in the quote currency.
    pub entry_fee: Decimal,
}

impl OpenPosition {
    #[must_use]
    pub fn entry_notional(&self) -> Decimal {
        self.entry_price * self.filled_quantity
    }
}

#[cfg(test)]
mod tests {
    use super::Side;

    #[test]
    fn closing_side_is_always_the_opposite_direction() {
        assert_eq!(Side::Long.closing_side(), Side::Short);
        assert_eq!(Side::Short.closing_side(), Side::Long);
    }
}
