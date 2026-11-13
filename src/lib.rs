//! Shared, execution-mode-independent trading domain.
//!
//! All execution backends must use the same strategy, risk, position, and trade
//! reporting code. Only market-data and order-execution adapters may vary by mode.

pub mod backtest;
pub mod config;
pub mod domain;
pub mod market_data;
pub mod risk;
