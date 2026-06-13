//! Runnable demo: drives the EventCore bank against a real PostgreSQL event
//! store.
//!
//! It opens two accounts, deposits into them, performs a multi-stream atomic
//! transfer, then runs a projection to build a read model and prints the
//! resulting balances and transaction log.
//!
//! Configure the database via `DATABASE_URL` (defaults to the local
//! docker-compose Postgres on port 5433). Run with:
//!
//! ```text
//! docker-compose up -d
//! cargo run -p eventcore-demo
//! ```

use std::env;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use eventcore::postgres::PostgresEventStore;
use eventcore::{ProjectionConfig, RetryPolicy, execute, run_projection};
use eventcore_demo::{
    AccountHolder, Deposit, MoneyAmount, OpenAccount, TransactionEntry, TransactionHistory,
    TransactionHistoryProjector, Transfer, new_account_id,
};

const DEFAULT_DATABASE_URL: &str = "postgres://postgres:postgres@localhost:5433/postgres";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("demo failed: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The scripted demo scenario. Returns an error instead of panicking so the
/// process exits cleanly with a readable message on failure.
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let database_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

    println!("EventCore bank demo");
    println!("Connecting to Postgres at {database_url}");

    let store = PostgresEventStore::new(database_url).await?;
    store.migrate().await;

    // Open two accounts on their own streams.
    let alice = new_account_id();
    let bob = new_account_id();

    execute_and_log(
        &store,
        OpenAccount {
            account_id: alice.clone(),
            holder: AccountHolder::try_new("Alice".to_string())?,
        },
        "Open account for Alice",
    )
    .await?;

    execute_and_log(
        &store,
        OpenAccount {
            account_id: bob.clone(),
            holder: AccountHolder::try_new("Bob".to_string())?,
        },
        "Open account for Bob",
    )
    .await?;

    // Fund the accounts.
    execute_and_log(
        &store,
        Deposit {
            account_id: alice.clone(),
            amount: MoneyAmount::try_new(1_000)?,
        },
        "Deposit 1000 cents into Alice's account",
    )
    .await?;

    execute_and_log(
        &store,
        Deposit {
            account_id: bob.clone(),
            amount: MoneyAmount::try_new(250)?,
        },
        "Deposit 250 cents into Bob's account",
    )
    .await?;

    // The centerpiece: a multi-stream atomic transfer.
    execute_and_log(
        &store,
        Transfer {
            from: alice.clone(),
            to: bob.clone(),
            amount: MoneyAmount::try_new(300)?,
        },
        "Transfer 300 cents from Alice to Bob (atomic, multi-stream)",
    )
    .await?;

    // Build the read model via projection (separate from the write models).
    let history = Arc::new(Mutex::new(TransactionHistory::default()));
    let projector = TransactionHistoryProjector::new(history.clone());
    run_projection(projector, &store, ProjectionConfig::default()).await?;

    let history = history
        .lock()
        .map_err(|_| "transaction-history mutex was poisoned")?;

    print_report(&history);

    Ok(())
}

/// Execute a command and print a one-line confirmation.
async fn execute_and_log<C>(
    store: &PostgresEventStore,
    command: C,
    label: &str,
) -> Result<(), eventcore::CommandError>
where
    C: eventcore::CommandLogic<Event = eventcore_demo::BankEvent>,
{
    let response = execute(store, command, RetryPolicy::new()).await?;
    println!("  ok: {label} (attempts: {})", response.attempts());
    Ok(())
}

/// Print the projected balances and transaction log.
fn print_report(history: &TransactionHistory) {
    println!();
    println!("Account balances (from projection):");
    for account_id in history.account_ids() {
        let holder = history
            .holder_of(account_id)
            .map(AccountHolder::as_ref)
            .unwrap_or("<unknown>");
        let balance = history
            .balance_of(account_id)
            .map(|amount| {
                let cents: u32 = amount.into();
                cents.to_string()
            })
            .unwrap_or_else(|| "0".to_string());
        println!("  {holder} ({account_id}): {balance} cents");
    }

    println!();
    println!("Transaction log (in stream order):");
    for entry in history.entries() {
        match entry {
            TransactionEntry::Opened { account_id, holder } => {
                println!("  opened   {} for {}", account_id, holder.as_ref());
            }
            TransactionEntry::Deposited { account_id, amount } => {
                let cents: u32 = (*amount).into();
                println!("  deposit  {account_id} += {cents} cents");
            }
            TransactionEntry::Withdrawn { account_id, amount } => {
                let cents: u32 = (*amount).into();
                println!("  withdraw {account_id} -= {cents} cents");
            }
        }
    }

    println!();
    println!("Total money in system: {} cents", history.total_balance());
}
