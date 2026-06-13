//! Integration tests for the EventCore bank demo, exercised through the
//! public API only (`execute`, `run_projection`, command derive macros, the
//! `Projector` trait). These tests double as usage examples for downstream
//! consumers.
//!
//! They run against the in-memory store so they are fast and deterministic in
//! CI. A separate Postgres-backed test proves the same flows against the
//! production backend.

use std::sync::{Arc, Mutex};

use eventcore::{ProjectionConfig, RetryPolicy, StreamId, execute, run_projection};
use eventcore_demo::{
    AccountHolder, Deposit, MoneyAmount, OpenAccount, TransactionHistory, Transfer, Withdraw,
    new_account_id,
};
use eventcore_memory::InMemoryEventStore;
use uuid::Uuid;

// =============================================================================
// Test Helpers
// =============================================================================

fn account_holder(name: &str) -> AccountHolder {
    AccountHolder::try_new(name.to_string()).expect("valid account holder name")
}

fn amount(cents: u32) -> MoneyAmount {
    MoneyAmount::try_new(cents).expect("valid money amount")
}

/// Collect every transaction-history read model from the store via projection.
///
/// This demonstrates the read-model code path: it is entirely separate from
/// the commands' write-model `apply`/`handle` methods.
async fn project_history(store: &InMemoryEventStore) -> TransactionHistory {
    let history = Arc::new(Mutex::new(TransactionHistory::default()));
    let projector = eventcore_demo::TransactionHistoryProjector::new(history.clone());
    run_projection(projector, store, ProjectionConfig::default())
        .await
        .expect("projection to complete");
    let guard = history.lock().expect("history mutex not poisoned");
    guard.clone()
}

// =============================================================================
// Integration Tests
// =============================================================================

/// Scenario: open an account, deposit, then withdraw within balance.
///
/// Given: a fresh in-memory store and a new account stream
/// When: the account is opened, 100 cents deposited, then 40 withdrawn
/// Then: every command succeeds and the projected balance is 60 cents
#[tokio::test]
async fn open_deposit_withdraw_happy_path() {
    // Given: an in-memory store and a new account id
    let store = InMemoryEventStore::new();
    let account_id = new_account_id();

    // When: the account is opened
    let _ = execute(
        &store,
        OpenAccount {
            account_id: account_id.clone(),
            holder: account_holder("Ada Lovelace"),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("open account to succeed");

    // And: 100 cents are deposited
    let _ = execute(
        &store,
        Deposit {
            account_id: account_id.clone(),
            amount: amount(100),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("deposit to succeed");

    // And: 40 cents are withdrawn
    let _ = execute(
        &store,
        Withdraw {
            account_id: account_id.clone(),
            amount: amount(40),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("withdraw to succeed");

    // Then: the projected balance reflects all three operations
    let history = project_history(&store).await;
    assert_eq!(
        history.balance_of(&account_id),
        Some(amount(60)),
        "balance should be 100 - 40 = 60 cents"
    );
}

/// Scenario: withdrawing more than the balance is rejected.
///
/// Given: an account opened with a 50-cent deposit
/// When: a 100-cent withdrawal is attempted
/// Then: a business-rule violation is returned and the balance is unchanged
#[tokio::test]
async fn withdraw_exceeding_balance_is_rejected() {
    // Given: an opened account funded with 50 cents
    let store = InMemoryEventStore::new();
    let account_id = new_account_id();

    let _ = execute(
        &store,
        OpenAccount {
            account_id: account_id.clone(),
            holder: account_holder("Grace Hopper"),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("open account to succeed");

    let _ = execute(
        &store,
        Deposit {
            account_id: account_id.clone(),
            amount: amount(50),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("deposit to succeed");

    // When: a withdrawal larger than the balance is attempted
    let result = execute(
        &store,
        Withdraw {
            account_id: account_id.clone(),
            amount: amount(100),
        },
        RetryPolicy::new(),
    )
    .await;

    // Then: it is rejected as a business-rule violation
    let error = result.expect_err("withdrawal should be rejected");
    assert!(
        matches!(error, eventcore::CommandError::BusinessRuleViolation(_)),
        "expected BusinessRuleViolation, got: {error:?}"
    );

    // And: the balance is unchanged (still 50 cents)
    let history = project_history(&store).await;
    assert_eq!(
        history.balance_of(&account_id),
        Some(amount(50)),
        "rejected withdrawal must not change the balance"
    );
}

/// Scenario: depositing into an account that was never opened is rejected.
///
/// Given: a fresh store with no opened account
/// When: a deposit is attempted against an unknown account
/// Then: a business-rule violation is returned
#[tokio::test]
async fn deposit_into_unopened_account_is_rejected() {
    // Given: an in-memory store with no account opened
    let store = InMemoryEventStore::new();
    let account_id = new_account_id();

    // When: a deposit is attempted
    let result = execute(
        &store,
        Deposit {
            account_id: account_id.clone(),
            amount: amount(25),
        },
        RetryPolicy::new(),
    )
    .await;

    // Then: it is rejected because the account is not open
    let error = result.expect_err("deposit into unopened account should be rejected");
    assert!(
        matches!(error, eventcore::CommandError::BusinessRuleViolation(_)),
        "expected BusinessRuleViolation, got: {error:?}"
    );
}

/// Scenario (centerpiece): a multi-stream atomic transfer moves money between
/// two accounts in a single `execute()` call.
///
/// Given: two opened accounts (source: 100, destination: 20)
/// When: 30 cents are transferred from source to destination
/// Then: both balances reflect the transfer (source 70, destination 50)
/// And: total money in the system is conserved
#[tokio::test]
async fn transfer_moves_money_atomically_between_two_streams() {
    // Given: two opened, funded accounts
    let store = InMemoryEventStore::new();
    let source = new_account_id();
    let destination = new_account_id();

    open_and_fund(&store, &source, "Source Holder", 100).await;
    open_and_fund(&store, &destination, "Destination Holder", 20).await;

    // When: a transfer debits the source and credits the destination atomically
    let _ = execute(
        &store,
        Transfer {
            from: source.clone(),
            to: destination.clone(),
            amount: amount(30),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("transfer to succeed");

    // Then: both balances reflect the transfer
    let history = project_history(&store).await;
    assert_eq!(
        history.balance_of(&source),
        Some(amount(70)),
        "source balance should be 100 - 30 = 70"
    );
    assert_eq!(
        history.balance_of(&destination),
        Some(amount(50)),
        "destination balance should be 20 + 30 = 50"
    );

    // And: total money is conserved
    assert_eq!(
        history.total_balance(),
        120,
        "total money across all accounts is conserved"
    );
}

/// Scenario: a transfer that would overdraw the source leaves BOTH streams
/// unchanged — proving the atomic all-or-nothing guarantee.
///
/// Given: source funded with 10 cents, destination funded with 5 cents
/// When: a 50-cent transfer is attempted
/// Then: it is rejected and both balances are unchanged
#[tokio::test]
async fn failed_transfer_leaves_both_streams_unchanged() {
    // Given: a source with insufficient funds and a destination
    let store = InMemoryEventStore::new();
    let source = new_account_id();
    let destination = new_account_id();

    open_and_fund(&store, &source, "Poor Source", 10).await;
    open_and_fund(&store, &destination, "Lucky Destination", 5).await;

    // When: a transfer larger than the source balance is attempted
    let result = execute(
        &store,
        Transfer {
            from: source.clone(),
            to: destination.clone(),
            amount: amount(50),
        },
        RetryPolicy::new(),
    )
    .await;

    // Then: the transfer is rejected
    let error = result.expect_err("overdrawing transfer should be rejected");
    assert!(
        matches!(error, eventcore::CommandError::BusinessRuleViolation(_)),
        "expected BusinessRuleViolation, got: {error:?}"
    );

    // And: neither stream changed
    let history = project_history(&store).await;
    assert_eq!(
        history.balance_of(&source),
        Some(amount(10)),
        "source balance must be unchanged after a failed transfer"
    );
    assert_eq!(
        history.balance_of(&destination),
        Some(amount(5)),
        "destination balance must be unchanged after a failed transfer"
    );
}

// =============================================================================
// Shared Helpers
// =============================================================================

async fn open_and_fund(
    store: &InMemoryEventStore,
    account_id: &StreamId,
    holder: &str,
    initial_cents: u32,
) {
    let _ = execute(
        store,
        OpenAccount {
            account_id: account_id.clone(),
            holder: account_holder(holder),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("open account to succeed");

    let _ = execute(
        store,
        Deposit {
            account_id: account_id.clone(),
            amount: amount(initial_cents),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("initial deposit to succeed");
}

/// Sanity check that distinct account ids are generated per call.
#[test]
fn new_account_id_is_unique() {
    let first = new_account_id();
    let second = new_account_id();
    assert_ne!(first, second, "each generated account id must be unique");
    // And it parses as a UUID, matching the documented format.
    let _ = Uuid::parse_str(first.as_ref()).expect("account id is a uuid");
}
