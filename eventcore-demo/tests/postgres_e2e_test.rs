//! End-to-end test proving the demo's public API works against the real
//! PostgreSQL backend, not just the in-memory store.
//!
//! Connects to the shared docker-compose Postgres (configurable via
//! `POSTGRES_HOST`/`POSTGRES_PORT`, defaulting to `localhost:5433`). Stream ids
//! are freshly generated per run so the test is isolated from prior runs that
//! share the same database.
//!
//! Run locally with `docker-compose up -d` first; CI provides Postgres as a
//! service container.

use std::env;
use std::sync::{Arc, Mutex};

use eventcore::postgres::PostgresEventStore;
use eventcore::{ProjectionConfig, RetryPolicy, execute, run_projection};
use eventcore_demo::{
    AccountHolder, Deposit, MoneyAmount, OpenAccount, TransactionHistory,
    TransactionHistoryProjector, Transfer, new_account_id,
};

fn connection_string() -> String {
    let host = env::var("POSTGRES_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = env::var("POSTGRES_PORT").unwrap_or_else(|_| "5433".to_string());
    format!("postgres://postgres:postgres@{host}:{port}/postgres")
}

async fn connect() -> PostgresEventStore {
    let store = PostgresEventStore::new(connection_string())
        .await
        .expect("connect to the docker-compose Postgres on :5433");
    // migrate() is idempotent — safe to run on every test invocation.
    store.migrate().await;
    store
}

fn holder(name: &str) -> AccountHolder {
    AccountHolder::try_new(name.to_string()).expect("valid account holder")
}

fn amount(cents: u32) -> MoneyAmount {
    MoneyAmount::try_new(cents).expect("valid money amount")
}

/// Scenario: the full open → deposit → atomic transfer flow persists correctly
/// to Postgres and is observable through a projection.
///
/// Given: two freshly-identified accounts in the shared Postgres store
/// When: both are opened, funded, and 30 cents transferred atomically
/// Then: the projected balances reflect the transfer and money is conserved
#[tokio::test]
async fn bank_flow_persists_through_postgres() {
    // Given: a connected Postgres store with migrations applied
    let store = connect().await;
    let source = new_account_id();
    let destination = new_account_id();

    // When: both accounts are opened and funded
    let _ = execute(
        &store,
        OpenAccount {
            account_id: source.clone(),
            holder: holder("Source"),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("open source account");

    let _ = execute(
        &store,
        OpenAccount {
            account_id: destination.clone(),
            holder: holder("Destination"),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("open destination account");

    let _ = execute(
        &store,
        Deposit {
            account_id: source.clone(),
            amount: amount(100),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("fund source account");

    let _ = execute(
        &store,
        Deposit {
            account_id: destination.clone(),
            amount: amount(20),
        },
        RetryPolicy::new(),
    )
    .await
    .expect("fund destination account");

    // And: 30 cents are transferred atomically across both streams
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
    .expect("atomic transfer to succeed");

    // Then: a projection over the persisted events reports the new balances
    let history = Arc::new(Mutex::new(TransactionHistory::default()));
    let projector = TransactionHistoryProjector::new(history.clone());
    run_projection(projector, &store, ProjectionConfig::default())
        .await
        .expect("projection to complete");

    let history = history.lock().expect("history mutex not poisoned");
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
}
