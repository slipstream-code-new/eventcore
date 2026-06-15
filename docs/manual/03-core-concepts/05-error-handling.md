# Chapter 3.5: Error Handling

Error handling in EventCore is designed to be explicit, recoverable, and informative. This chapter covers error types, handling strategies, and best practices for building resilient event-sourced systems.

## Error Philosophy

EventCore follows these principles:

1. **Errors are values** - Use `Result<T, E>` everywhere
2. **Be specific** - Different error types for different failures
3. **Fail fast** - Validate early in the command pipeline
4. **Recover gracefully** - Automatic retries for transient errors
5. **Provide context** - Rich error messages for debugging

## Error Types

### Command Errors

The main error type for command execution:

```rust
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// A business rule was violated. Wraps the command's own typed error;
    /// this is what `require!` and your typed error enums convert into.
    #[error(transparent)]
    BusinessRuleViolation(Box<dyn std::error::Error + Send + Sync>),

    /// Optimistic concurrency conflicts persisted after the retry policy was
    /// exhausted. The `u32` is the number of retry attempts made.
    #[error("concurrency conflict after {0} retry attempts")]
    ConcurrencyError(u32),

    /// The underlying event store failed.
    #[error("event store error: {0}")]
    EventStoreError(EventStoreError),

    /// A validation error surfaced during execution.
    #[error("validation error: {0}")]
    ValidationError(String),
}
```

> See [Chapter 8.3: Error Reference](../08-reference/03-error-reference.md) for
> a per-variant breakdown of causes and resolutions.

### Event Store Errors

Storage-specific errors:

```rust
#[derive(Debug, thiserror::Error)]
pub enum EventStoreError {
    /// A stream was assigned multiple different expected versions in one batch.
    #[error("conflicting expected versions for stream {stream_id}: first={first_version}, second={second_version}")]
    ConflictingExpectedVersions {
        stream_id: StreamId,
        first_version: StreamVersion,
        second_version: StreamVersion,
    },

    /// An append targeted a stream that was not declared with an expected version.
    #[error("stream {stream_id} must be registered before appending events")]
    UndeclaredStream { stream_id: StreamId },

    /// Event serialization failed before persistence.
    #[error("failed to serialize event for stream {stream_id}: {detail}")]
    SerializationFailed { stream_id: StreamId, detail: String },

    /// Stored event payloads could not be deserialized into the requested type.
    #[error("failed to deserialize event for stream {stream_id}: {detail}")]
    DeserializationFailed { stream_id: StreamId, detail: String },

    /// Infrastructure failure surfaced by the backing store.
    #[error("{operation} operation failed")]
    StoreFailure { operation: Operation },

    /// Optimistic concurrency conflict: the expected version no longer matches.
    #[error("version conflict on stream {stream_id}: expected version {expected}, found {actual}")]
    VersionConflict {
        stream_id: StreamId,
        expected: StreamVersion,
        actual: StreamVersion,
    },
}
```

## Validation Patterns

### Using the `require!` Macro

The `require!` macro makes validation concise:

```rust
use eventcore::require;

fn handle(&self, state: Self::State) -> Result<NewEvents<Self::Event>, CommandError> {
    // Simple validation
    require!(self.amount > 0, "Amount must be positive");

    // Validation with formatting
    require!(
        state.balance >= self.amount,
        "Insufficient balance: have {}, need {}",
        state.balance,
        self.amount
    );

    // Complex validation
    require!(
        state.account.is_active && !state.account.is_frozen,
        "Account must be active and not frozen"
    );

    // Continue with business logic...
    Ok(NewEvents::from(vec![/* events */]))
}
```

### Custom Validation Functions

For complex validations:

```rust
impl TransferMoney {
    fn validate_business_rules(&self, state: &AccountState) -> Result<(), CommandError> {
        // Daily limit check
        self.validate_daily_limit(state)?;

        // Fraud check
        self.validate_fraud_rules(state)?;

        // Compliance check
        self.validate_compliance(state)?;

        Ok(())
    }

    fn validate_daily_limit(&self, state: &AccountState) -> Result<(), CommandError> {
        const DAILY_LIMIT: Money = Money::from_cents(50_000_00);

        let today_total = state.transfers_today() + self.amount;
        require!(
            today_total <= DAILY_LIMIT,
            "Daily transfer limit exceeded: {} > {}",
            today_total,
            DAILY_LIMIT
        );

        Ok(())
    }
}

// In handle()
fn handle(&self, state: Self::State) -> Result<NewEvents<Self::Event>, CommandError> {
    // Run all validations
    self.validate_business_rules(&state)?;

    // Generate events...
    Ok(NewEvents::from(vec![]))
}
```

### Type-Safe Validation

Use types to make invalid states unrepresentable:

```rust
use nutype::nutype;

// Email validation at type level
#[nutype(
    sanitize(lowercase, trim),
    validate(regex = r"^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}$"),
    derive(Debug, Clone, Serialize, Deserialize)
)]
pub struct Email(String);

// Money that can't be negative
#[nutype(
    validate(greater_or_equal = 0),
    derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)
)]
pub struct Money(u64);

// Now these validations happen at construction
let email = Email::try_new("invalid-email")?; // Fails at parse time
let amount = Money::try_new(-100)?; // Compile error - u64 can't be negative
```

## Handling Transient Errors

### Automatic Retries

EventCore automatically retries on version conflicts:

```rust
// EventCore handles retries via RetryPolicy:
let policy = RetryPolicy::new()
    .max_retries(5)
    .backoff_strategy(BackoffStrategy::Exponential {
        base_ms: DelayMilliseconds::new(100),
    });

// execute() automatically retries on version conflicts
// using the configured policy
execute(&store, command, policy).await?;
```

### Circuit Breaker Pattern

Protect against cascading failures:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub struct CircuitBreaker {
    failure_count: AtomicU32,
    last_failure_time: AtomicU64,
    threshold: u32,
    timeout: Duration,
}

impl CircuitBreaker {
    pub fn call<F, T, E>(&self, f: F) -> Result<T, CircuitBreakerError<E>>
    where
        F: FnOnce() -> Result<T, E>,
    {
        // Check if circuit is open
        if self.is_open() {
            return Err(CircuitBreakerError::Open);
        }

        // Try the operation
        match f() {
            Ok(result) => {
                self.on_success();
                Ok(result)
            }
            Err(e) => {
                self.on_failure();
                Err(CircuitBreakerError::Failed(e))
            }
        }
    }

    fn is_open(&self) -> bool {
        let failures = self.failure_count.load(Ordering::Relaxed);
        if failures >= self.threshold {
            let last_failure = self.last_failure_time.load(Ordering::Relaxed);
            let elapsed = Duration::from_millis(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64 - last_failure
            );
            elapsed < self.timeout
        } else {
            false
        }
    }
}

// Illustrative: wrap a store in your own circuit-breaker adapter.
// `read_stream` is a trait method on EventStore returning an EventStream<E>.
impl<S: EventStore> CircuitBreakerStore<S> {
    pub async fn read_stream_with_circuit_breaker<E: Event>(
        &self,
        stream_id: StreamId,
    ) -> Result<EventStream<E>, EventStoreError> {
        self.circuit_breaker.call(|| {
            self.inner.read_stream::<E>(stream_id)
        })
    }
}
```

## Error Recovery Strategies

### Compensating Commands

When things go wrong, emit compensating events:

```rust
#[derive(Command, Clone)]
struct RefundPayment {
    #[stream]
    payment: StreamId,

    #[stream]
    account: StreamId,

    reason: RefundReason,
}

fn handle(&self, state: Self::State) -> Result<NewEvents<Self::Event>, CommandError> {
    // Validate refund is possible
    require!(
        state.payment.status == PaymentStatus::Completed,
        "Can only refund completed payments"
    );

    require!(
        !state.payment.is_refunded,
        "Payment already refunded"
    );

    // Compensating events
    Ok(NewEvents::from(vec![
        PaymentEvent::Refunded { amount: state.payment.amount, reason: self.reason.clone() },
        AccountEvent::Credited { amount: state.payment.amount, reference: format!("Refund for payment {}", state.payment.id) },
    ]))
}
```

### Dead Letter Queues

Handle permanently failed commands:

```rust
pub struct DeadLetterQueue<C> {
    failed_commands: Vec<FailedCommand<C>>,
}

#[derive(Debug)]
pub struct FailedCommand<C> {
    pub command: C,
    pub error: CommandError,
    pub attempts: usize,
    pub first_attempted: DateTime<Utc>,
    pub last_attempted: DateTime<Utc>,
}

// Application-level DLQ wrapper around execute()
pub async fn execute_with_dlq<C, S>(
    store: &S,
    command: C,
    policy: RetryPolicy,
    dlq: &mut DeadLetterQueue<C>,
) -> Result<ExecutionResponse, CommandError>
where
    C: CommandLogic + CommandStreams + Clone,
    S: EventStore,
{
    match execute(store, command.clone(), policy).await {
        Ok(result) => Ok(result),
        Err(e) if e.is_permanent() => {
            // Add to DLQ for manual intervention
            dlq.add(FailedCommand {
                command,
                error: e.clone(),
                attempts: 5,
                first_attempted: Utc::now(),
                last_attempted: Utc::now(),
            });
            Err(e)
        }
        Err(e) => Err(e),
    }
}
```

## Error Context and Debugging

### Rich Error Context

Add context to errors:

```rust
use std::fmt;

#[derive(Debug)]
pub struct ErrorContext {
    pub command_type: &'static str,
    pub stream_ids: Vec<StreamId>,
    pub correlation_id: CorrelationId,
    pub user_id: Option<UserId>,
    pub additional_context: HashMap<String, String>,
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Command: {}, Streams: {:?}, Correlation: {}",
            self.command_type,
            self.stream_ids,
            self.correlation_id
        )?;

        if let Some(user) = &self.user_id {
            write!(f, ", User: {}", user)?;
        }

        for (key, value) in &self.additional_context {
            write!(f, ", {}: {}", key, value)?;
        }

        Ok(())
    }
}

// Wrap errors with context
pub type ContextualResult<T> = Result<T, ContextualError>;

#[derive(Debug, thiserror::Error)]
#[error("{context}\nError: {source}")]
pub struct ContextualError {
    #[source]
    source: CommandError,
    context: ErrorContext,
}
```

### Structured Logging

Log errors with full context:

```rust
use tracing::{error, warn, info, instrument};

#[instrument(skip(self, state))]
fn handle(&self, state: Self::State) -> Result<NewEvents<Self::Event>, CommandError> {
    info!(
        amount = %self.amount,
        from = %self.from_account,
        to = %self.to_account,
        "Processing transfer"
    );

    if let Err(e) = self.validate_business_rules(&state) {
        error!(
            error = %e,
            balance = %state.balance,
            daily_total = %state.transfers_today(),
            "Transfer validation failed"
        );
        return Err(e);
    }

    // Continue...
    Ok(NewEvents::from(vec![/* events */]))
}
```

## Testing Error Scenarios

### Unit Tests for Validation

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_insufficient_balance_error() {
        let command = TransferMoney {
            from_account: StreamId::try_new("account-1").unwrap(),
            to_account: StreamId::try_new("account-2").unwrap(),
            amount: Money::from_cents(1000),
        };

        let state = AccountState {
            balance: Money::from_cents(500),
            ..Default::default()
        };

        let result = command.validate_business_rules(&state);

        assert!(matches!(
            result,
            Err(CommandError::BusinessRuleViolation(err)) if err.to_string().contains("Insufficient balance")
        ));
    }

    #[tokio::test]
    async fn test_daily_limit_exceeded() {
        let command = TransferMoney {
            from_account: StreamId::try_new("account-1").unwrap(),
            to_account: StreamId::try_new("account-2").unwrap(),
            amount: Money::from_cents(10_000),
        };

        let mut state = AccountState::default();
        state.add_todays_transfer(Money::from_cents(45_000));

        let result = command.validate_business_rules(&state);

        assert!(matches!(
            result,
            Err(CommandError::BusinessRuleViolation(err)) if err.to_string().contains("Daily transfer limit")
        ));
    }
}
```

### Integration Tests for Concurrency

```rust
#[tokio::test]
async fn test_concurrent_modification_handling() {
    let store = InMemoryEventStore::new();
    let policy = RetryPolicy::new();

    // Setup
    create_account(&store, "account-1", 1000).await;

    // Create two conflicting commands
    let withdraw1 = WithdrawMoney {
        account: StreamId::try_new("account-1").unwrap(),
        amount: Money::from_cents(600),
    };

    let withdraw2 = WithdrawMoney {
        account: StreamId::try_new("account-1").unwrap(),
        amount: Money::from_cents(700),
    };

    // Execute concurrently
    let store_ref = &store;
    let (result1, result2) = tokio::join!(
        execute(store_ref, withdraw1, policy.clone()),
        execute(store_ref, withdraw2, policy)
    );

    // One should succeed, one should fail due to insufficient funds after retry
    let successes = [&result1, &result2]
        .iter()
        .filter(|r| r.is_ok())
        .count();

    assert_eq!(successes, 1, "Exactly one withdrawal should succeed");

    // Check final balance
    let balance = get_account_balance(&store, "account-1").await;
    assert!(balance == 400 || balance == 300); // 1000 - 600 or 1000 - 700
}
```

### Chaos Testing

```rust
use eventcore_testing::chaos::{ChaosConfig, ChaosEventStoreExt};

#[tokio::test]
async fn test_resilience_under_chaos() {
    let base_store = InMemoryEventStore::new();
    let chaos_store = base_store.with_chaos(
        ChaosConfig::default()
            .with_failure_probability(0.1) // 10% chance of failure
            .with_version_conflict_probability(0.2), // 20% chance of conflicts
    );

    let policy = RetryPolicy::new()
        .max_retries(10);

    // Run many operations
    let mut handles = vec![];
    for i in 0..100 {
        let store = chaos_store.clone();
        let policy = policy.clone();
        let handle = tokio::spawn(async move {
            let command = CreateTask {
                title: format!("Task {}", i),
                // ...
            };
            execute(&store, command, policy).await
        });
        handles.push(handle);
    }

    // Collect results
    let results: Vec<_> = futures::future::join_all(handles).await;

    // Despite chaos, most should succeed due to retries
    let success_rate = results.iter()
        .filter(|r| r.as_ref().unwrap().is_ok())
        .count() as f64 / results.len() as f64;

    assert!(success_rate > 0.95, "Success rate too low: {}", success_rate);
}
```

## Production Error Handling

### Monitoring and Alerting

```rust
use prometheus::{Counter, Histogram, register_counter, register_histogram};

lazy_static! {
    static ref COMMAND_ERRORS: Counter = register_counter!(
        "eventcore_command_errors_total",
        "Total number of command errors"
    ).unwrap();

    static ref RETRY_COUNT: Histogram = register_histogram!(
        "eventcore_command_retries",
        "Number of retries per command"
    ).unwrap();
}

// Application-level metrics wrapper around execute()
async fn execute_with_metrics<C, S>(
    store: &S,
    command: C,
    policy: RetryPolicy,
) -> Result<ExecutionResponse, CommandError>
where
    C: CommandLogic + CommandStreams,
    S: EventStore,
{
    COMMAND_COUNTER.inc();
    let timer = COMMAND_DURATION.start_timer();

    let result = execute(store, command, policy).await;

    timer.observe_duration();

    if result.is_err() {
        COMMAND_ERRORS.inc();
    }

    result
}
```

### Error Recovery Procedures

Document recovery procedures:

```rust
/// Recovery procedure for payment processing failures
///
/// 1. Check payment provider status
/// 2. Verify account balances match event history
/// 3. Look for orphaned payments in provider but not in events
/// 4. Run reconciliation command if discrepancies found
/// 5. Contact support if automated recovery fails
#[derive(Command, Clone)]
struct ReconcilePayments {
    #[stream]
    payment_provider: StreamId,

    #[stream]
    reconciliation_log: StreamId,

    provider_transactions: Vec<ProviderTransaction>,
}
```

## Best Practices

### 1. Fail Fast

Validate as early as possible:

```rust
// ✅ Good - validate at construction
impl TransferMoney {
    pub fn new(
        from: StreamId,
        to: StreamId,
        amount: Money,
    ) -> Result<Self, ValidationError> {
        if from == to {
            return Err(ValidationError::SameAccount);
        }

        Ok(Self {
            from_account: from,
            to_account: to,
            amount,
        })
    }
}

// ❌ Bad - validate late in handle()
```

### 2. Be Specific

Use specific error types:

```rust
// ✅ Good - specific errors
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("Insufficient balance: available {available}, requested {requested}")]
    InsufficientBalance { available: Money, requested: Money },

    #[error("Daily limit exceeded: limit {limit}, attempted {attempted}")]
    DailyLimitExceeded { limit: Money, attempted: Money },

    #[error("Account {0} is frozen")]
    AccountFrozen(AccountId),
}

// ❌ Bad - generic errors
Err("Transfer failed".into())
```

### 3. Make Errors Actionable

Provide enough context to fix issues:

```rust
// ✅ Good - actionable error
require!(
    state.account.kyc_verified,
    "Account KYC verification required. Please complete verification at: https://example.com/kyc/{}",
    state.account.id
);

// ❌ Bad - vague error
require!(state.account.kyc_verified, "KYC required");
```

## Summary

Error handling in EventCore:

- ✅ **Type-safe** - Errors encoded in function signatures
- ✅ **Recoverable** - Automatic retries for transient failures
- ✅ **Informative** - Rich context for debugging
- ✅ **Testable** - Easy to test error scenarios
- ✅ **Production-ready** - Monitoring and recovery built-in

Best practices:

1. Use `require!` macro for concise validation
2. Create specific error types for your domain
3. Add context to errors for debugging
4. Test error scenarios thoroughly
5. Monitor errors in production

You've completed Part 3! Continue to [Part 4: Building Web APIs](../04-building-web-apis/README.md) →
