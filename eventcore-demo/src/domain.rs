//! Domain vocabulary for the bank demo: semantic value types and the event
//! enum. Every concept is a named type — no raw primitives leak into the
//! domain. Primitives appear only at the serde / display IO boundary.

use eventcore::{Event, StreamId};
use nutype::nutype;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The name of the person or entity that holds an account.
///
/// Validated to be non-empty (after trimming surrounding whitespace) so that
/// an account always has a meaningful holder.
#[nutype(
    sanitize(trim),
    validate(not_empty),
    derive(Debug, Clone, PartialEq, Eq, AsRef, Serialize, Deserialize)
)]
pub struct AccountHolder(String);

/// A validated monetary amount in whole cents.
///
/// Construction rejects zero, so every deposit, withdrawal, and transfer moves
/// a strictly positive amount of money.
#[nutype(
    validate(greater = 0),
    derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Into,
        Serialize,
        Deserialize
    )
)]
pub struct MoneyAmount(u32);

/// Cross-type conversion used by read models that accumulate signed balances.
///
/// Encapsulating this on the domain type keeps callers from reaching for
/// `into_inner()` to do arithmetic.
impl From<MoneyAmount> for i64 {
    fn from(amount: MoneyAmount) -> Self {
        let cents: u32 = amount.into();
        i64::from(cents)
    }
}

/// Generate a fresh, unique account stream identifier.
///
/// Each bank account is its own event stream; a UUIDv7 keeps ids unique and
/// roughly time-ordered. A UUID's hyphenated string contains only `[0-9a-f-]`,
/// none of which are `StreamId` metacharacters, so construction here is
/// provably infallible — the `expect` documents that invariant and can never
/// fire at runtime.
pub fn new_account_id() -> StreamId {
    StreamId::try_new(Uuid::now_v7().to_string()).expect("a uuid string is always a valid StreamId")
}

/// Domain events for the bank demo.
///
/// Each variant carries the `StreamId` of the account it belongs to, which the
/// `Event` trait uses to route the event to the correct stream. The multi-stream
/// `Transfer` command emits `MoneyWithdrawn` on the source stream and
/// `MoneyDeposited` on the destination stream in a single atomic append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BankEvent {
    /// An account was opened with a named holder.
    AccountOpened {
        account_id: StreamId,
        holder: AccountHolder,
    },
    /// Money was deposited into an account.
    MoneyDeposited {
        account_id: StreamId,
        amount: MoneyAmount,
    },
    /// Money was withdrawn from an account.
    MoneyWithdrawn {
        account_id: StreamId,
        amount: MoneyAmount,
    },
}

impl Event for BankEvent {
    fn stream_id(&self) -> &StreamId {
        match self {
            BankEvent::AccountOpened { account_id, .. }
            | BankEvent::MoneyDeposited { account_id, .. }
            | BankEvent::MoneyWithdrawn { account_id, .. } => account_id,
        }
    }

    fn event_type_name() -> &'static str {
        "BankEvent"
    }
}
