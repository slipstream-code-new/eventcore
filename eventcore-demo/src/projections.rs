//! Read models (projections) for the bank demo.
//!
//! This is a SEPARATE code path from the command write models in
//! `commands.rs`. It folds the same events but for a different purpose:
//! presenting account balances and a transaction log to consumers. The fold
//! here is intentionally independent of `AccountState::apply` so the read and
//! write models can evolve without coupling.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use eventcore::{Projector, StreamId, StreamPosition};

use crate::domain::{AccountHolder, BankEvent, MoneyAmount};

/// A single recorded movement of money, for display in a transaction log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionEntry {
    Opened {
        account_id: StreamId,
        holder: AccountHolder,
    },
    Deposited {
        account_id: StreamId,
        amount: MoneyAmount,
    },
    Withdrawn {
        account_id: StreamId,
        amount: MoneyAmount,
    },
}

/// Read model: current balances per account plus an ordered transaction log.
///
/// Balances are stored as signed cents internally to keep the fold simple;
/// callers read them back as validated `MoneyAmount` values through methods.
#[derive(Debug, Default, Clone)]
pub struct TransactionHistory {
    balances: HashMap<StreamId, i64>,
    holders: HashMap<StreamId, AccountHolder>,
    log: Vec<TransactionEntry>,
}

impl TransactionHistory {
    /// Fold a single event into the read model.
    fn record(&mut self, event: BankEvent) {
        match event {
            BankEvent::AccountOpened { account_id, holder } => {
                let _ = self.balances.entry(account_id.clone()).or_insert(0);
                let _ = self.holders.insert(account_id.clone(), holder.clone());
                self.log
                    .push(TransactionEntry::Opened { account_id, holder });
            }
            BankEvent::MoneyDeposited { account_id, amount } => {
                let entry = self.balances.entry(account_id.clone()).or_insert(0);
                *entry += i64::from(amount);
                self.log
                    .push(TransactionEntry::Deposited { account_id, amount });
            }
            BankEvent::MoneyWithdrawn { account_id, amount } => {
                let entry = self.balances.entry(account_id.clone()).or_insert(0);
                *entry -= i64::from(amount);
                self.log
                    .push(TransactionEntry::Withdrawn { account_id, amount });
            }
        }
    }

    /// Current balance of an account, if it has been opened.
    ///
    /// Returns `None` for accounts that were never opened, and a positive
    /// `MoneyAmount` for accounts with a non-zero balance. A zero balance maps
    /// to `None` because `MoneyAmount` is strictly positive.
    pub fn balance_of(&self, account_id: &StreamId) -> Option<MoneyAmount> {
        let cents = *self.balances.get(account_id)?;
        if cents <= 0 {
            return None;
        }
        u32::try_from(cents)
            .ok()
            .and_then(|c| MoneyAmount::try_new(c).ok())
    }

    /// The holder of an account, if it has been opened.
    pub fn holder_of(&self, account_id: &StreamId) -> Option<&AccountHolder> {
        self.holders.get(account_id)
    }

    /// Sum of all account balances in cents — useful for proving that
    /// transfers conserve money across the system.
    pub fn total_balance(&self) -> i64 {
        self.balances.values().copied().sum()
    }

    /// All known account ids.
    pub fn account_ids(&self) -> impl Iterator<Item = &StreamId> {
        self.balances.keys()
    }

    /// The ordered transaction log.
    pub fn entries(&self) -> &[TransactionEntry] {
        &self.log
    }
}

/// A `Projector` that maintains a shared `TransactionHistory` read model.
///
/// The history is held behind `Arc<Mutex<_>>` so the caller retains a handle
/// to inspect the result after `run_projection` returns.
pub struct TransactionHistoryProjector {
    history: Arc<Mutex<TransactionHistory>>,
}

impl TransactionHistoryProjector {
    pub fn new(history: Arc<Mutex<TransactionHistory>>) -> Self {
        Self { history }
    }
}

impl Projector for TransactionHistoryProjector {
    type Event = BankEvent;
    type Error = Infallible;
    type Context = ();

    fn apply(
        &mut self,
        event: Self::Event,
        _position: StreamPosition,
        _ctx: &mut Self::Context,
    ) -> Result<(), Self::Error> {
        if let Ok(mut history) = self.history.lock() {
            history.record(event);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "bank-transaction-history"
    }
}
