//! # EventCore bank demo
//!
//! A small, runnable demonstration of [EventCore](https://github.com/jwilger/eventcore)
//! with the PostgreSQL backend. It models a bank with three account-level
//! commands ([`OpenAccount`], [`Deposit`], [`Withdraw`]) plus a multi-stream
//! atomic [`Transfer`] — the centerpiece showing EventCore's signature
//! capability of writing several event streams in one optimistic-concurrency
//! transaction.
//!
//! Read models live in a separate code path: [`TransactionHistory`] is built
//! by the [`TransactionHistoryProjector`] via `eventcore::run_projection`,
//! never by reusing a command's write-model `apply`.
//!
//! The binary (`src/main.rs`) wires these against a real `PostgresEventStore`;
//! the integration tests exercise the same public API against the in-memory
//! store.

mod commands;
mod domain;
mod projections;

pub use commands::{
    Deposit, DepositError, OpenAccount, OpenAccountError, Transfer, TransferError, Withdraw,
    WithdrawError,
};
pub use domain::{AccountHolder, BankEvent, MoneyAmount, new_account_id};
pub use projections::{TransactionEntry, TransactionHistory, TransactionHistoryProjector};
