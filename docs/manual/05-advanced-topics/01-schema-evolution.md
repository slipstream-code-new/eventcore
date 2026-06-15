# Chapter 5.1: Schema Evolution

Schema evolution is the process of changing event structures over time while
still being able to replay historical events. In event sourcing you can never
change historical events — they're immutable facts about what happened — so the
challenge is making _new_ code able to read _old_ events.

EventCore stores events as JSON via serde, and deserialization is driven
entirely by the JSON structure. That gives you two complementary mechanisms,
both standard serde/Rust features rather than a bespoke EventCore subsystem:

- **serde field defaults** for backwards-compatible (additive) changes, and
- **new enum variants** for incompatible changes.

This is the strategy recorded in
[ADR-0035](../../adr/ADR-0035-event-schema-evolution-via-enum-variants.md).
EventCore deliberately does **not** ship an upcasting registry, version
metadata, or a migration framework — see the end of this chapter for why.

## The Challenge

Your system evolves. Business requirements change. Data structures need to
adapt. But the events already written to the store were serialized against the
_old_ shape:

```rust
// Day 1: Simple user registration
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserRegistered {
    user_id: UserId,
    email: Email,
}

// 6 months later: you need more fields - but old events don't have them!
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserRegistered {
    user_id: UserId,
    email: Email,
    first_name: FirstName,
    last_name: LastName,
    preferences: UserPreferences,
}
```

If you simply add required fields, every historical `UserRegistered` event will
fail to deserialize. The rest of this chapter shows how to evolve without
breaking replay.

## EventCore's Schema Evolution Approach

EventCore relies on exactly two mechanisms, both built on serde and Rust enums:

1. **serde defaults** — add backwards-compatible fields that old events can omit.
2. **New enum variants** — represent incompatible changes as additional event
   variants that coexist with the originals.

The rules from ADR-0035 govern how variants are handled:

- Old variants are **never removed** — they represent historical facts.
- `CommandLogic::apply()` **handles all variants** — pattern matching covers
  every version of the event.
- `CommandLogic::handle()` **emits only the latest variant** — new events use
  the current schema.
- Projectors **handle all variants** — read models must process the full
  history.

Anything beyond these two mechanisms (registries, migration traits, lazy
upcasting) is _application-level code you choose to write_, not an EventCore
feature. The examples in this chapter that show such patterns are explicitly
labeled application-level.

## Backward Compatible Changes

These changes don't break existing events, because old JSON still deserializes.

### Adding Optional Fields

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserRegistered {
    user_id: UserId,
    email: Email,

    // New optional fields with defaults - old events deserialize as None/default
    #[serde(default)]
    first_name: Option<FirstName>,

    #[serde(default)]
    last_name: Option<LastName>,

    #[serde(default)]
    preferences: UserPreferences,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            newsletter: false,
            notifications: true,
            theme: Theme::Light,
        }
    }
}
```

### Adding Fields with Sensible Defaults

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrderPlaced {
    order_id: OrderId,
    customer_id: CustomerId,
    items: Vec<OrderItem>,

    // New field with a computed default
    #[serde(default = "default_currency")]
    currency: Currency,
}

fn default_currency() -> Currency {
    Currency::USD
}
```

> Avoid defaults that capture _replay_ time rather than _record_ time. A
> `#[serde(default = "Utc::now")]` on an event field, for example, would stamp
> historical events with the time they were replayed, not the time they
> happened. If a value was genuinely unknown when the event was recorded, model
> it as `Option<T>` defaulting to `None`.

### Adding Enum Variants Inside a Payload

A field that is itself an enum can gain new variants. Old events still
deserialize because they only ever contained the original variants:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum PaymentMethod {
    CreditCard { last_four: CardLastFour },
    BankTransfer { account: AccountNumber },
    PayPal { email: Email },

    // New variants - old events never contained these, new events may
    ApplePay { device_id: DeviceId },
    GooglePay { account_id: AccountId },
}
```

## Incompatible Changes

Field removal, field-type changes, and restructuring all break direct
deserialization of historical events. Per ADR-0035, the answer is **not** to
rewrite the old events — it is to introduce a new event variant alongside the
old one. The old variant keeps deserializing historical data; the new variant
carries the new shape.

The sections below describe the kinds of change that force a new variant. The
"Evolving the Event Enum" section then shows how to express them.

### Removing a Field

```rust
// Original shape - still present in the store
//   { user_id, email, username }
//
// New shape - username dropped
//   { user_id, email }
```

`username` cannot simply disappear from a single struct: old JSON contains it,
new JSON does not, and the two cannot share one struct without making the field
optional. If the field is genuinely gone, add a new variant.

### Changing a Field's Type

```rust
// Original: user_id stored as a bare string
//   { user_id: "abc", email }
//
// New: user_id stored as a structured UserId
//   { user_id: { ... }, email }
```

A type change is incompatible — old JSON won't parse into the new type. Add a
new variant.

### Restructuring Data

```rust
// Original: flat billing/shipping fields
//   { order_id, billing_street, billing_city, shipping_street, shipping_city }
//
// New: nested Address values
//   { order_id, billing_address: { ... }, shipping_address: { ... } }
```

Restructuring is incompatible. Add a new variant.

## Evolving the Event Enum

In EventCore, an application's domain events are a single enum (the
`CommandLogic::Event` associated type). serde uses its default
externally-tagged format, so the variant name is the JSON key. When a schema
changes incompatibly, you add a new variant — the old variant continues to
deserialize historical events unchanged.

```rust
use eventcore::{Event, StreamId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum UserEvent {
    // Original variant - still deserializes from historical events
    Registered {
        stream: StreamId,
        user_id: UserId,
        email: Email,
        username: Username,
    },

    // New variant after dropping `username` and splitting the name out
    RegisteredV2 {
        stream: StreamId,
        user_id: UserId,
        email: Email,
        first_name: FirstName,
        last_name: LastName,
    },

    // A later, further-restructured variant
    RegisteredV3 {
        stream: StreamId,
        user_id: UserId,
        email: Email,
        profile: UserProfile,
    },
}

impl Event for UserEvent {
    fn stream_id(&self) -> &StreamId {
        match self {
            UserEvent::Registered { stream, .. }
            | UserEvent::RegisteredV2 { stream, .. }
            | UserEvent::RegisteredV3 { stream, .. } => stream,
        }
    }

    // A stable, module-independent name used only for the `event_type` storage
    // column (auditing/debugging - NOT used for deserialization). It is a single
    // static name for the whole enum, not per-variant.
    fn event_type_name() -> &'static str {
        "UserEvent"
    }
}
```

> The `Event` trait requires exactly two methods: `stream_id(&self) ->
&StreamId` (each variant reports its stream) and the static
> `event_type_name() -> &'static str` (a single stable name for the whole enum,
> used only for the storage `event_type` column). See `cargo doc -p eventcore`
> for the exact trait. The key point is that _every_ variant — old and new — is
> part of the enum forever.

There is no migration step and no version registry. Old and new events coexist
naturally because each is just a different shape of the same enum.

## State Reconstruction Across Versions

Because the event enum carries every historical variant, `CommandLogic::apply`
must match all of them. This is the write-model (command-state) code path, which
EventCore keeps separate from read-model (projection) code. `apply` takes the
accumulated state and a borrowed event and returns the new state:

```rust
use eventcore::{CommandError, CommandLogic, NewEvents};

#[derive(Default)]
struct UserState {
    exists: bool,
    email: Option<Email>,
    first_name: Option<FirstName>,
    last_name: Option<LastName>,
    profile: Option<UserProfile>,
}

impl CommandLogic for CreateUser {
    type State = UserState;
    type Event = UserEvent;

    // Folds events into state. Every variant - including obsolete ones - is
    // handled so historical streams replay correctly.
    fn apply(&self, mut state: Self::State, event: &Self::Event) -> Self::State {
        match event {
            UserEvent::Registered { email, .. } => {
                state.exists = true;
                state.email = Some(email.clone());
                // Legacy events have no separate names
                state.first_name = None;
                state.last_name = None;
            }
            UserEvent::RegisteredV2 { email, first_name, last_name, .. } => {
                state.exists = true;
                state.email = Some(email.clone());
                state.first_name = Some(first_name.clone());
                state.last_name = Some(last_name.clone());
            }
            UserEvent::RegisteredV3 { email, profile, .. } => {
                state.exists = true;
                state.email = Some(email.clone());
                state.first_name = Some(profile.first_name().clone());
                state.last_name = Some(profile.last_name().clone());
                state.profile = Some(profile.clone());
            }
        }
        state
    }

    // Emits only the latest variant - new events always use the current schema.
    fn handle(&self, state: Self::State) -> Result<NewEvents<Self::Event>, CommandError> {
        // ... validate preconditions against `state`, then produce events ...
        # todo!()
    }
}
```

Because Rust enum matching is exhaustive, adding a new variant forces you to
update every `apply` (and every projector) that consumes the enum — the
compiler will not let you forget an old version.

## Projection Evolution

Read models must also process the full history, so a projector matches all
variants too. In EventCore, a read model implements the `Projector` trait. Its
`apply` method receives the event **by value**, the event's
`StreamPosition`, and a mutable context, and returns `Result<(), Self::Error>`.

```rust
use eventcore::{Projector, StreamPosition};

# struct UserListProjection;
# struct UserSummary;
# struct ProjectionContext;
# #[derive(Debug)] struct UserListError;
impl Projector for UserListProjection {
    type Event = UserEvent;
    type Error = UserListError;
    type Context = ProjectionContext;

    fn apply(
        &mut self,
        event: Self::Event,
        _position: StreamPosition,
        ctx: &mut Self::Context,
    ) -> Result<(), Self::Error> {
        match event {
            // Handle all versions of user registration
            UserEvent::Registered { user_id, email, username } => {
                ctx.upsert(UserSummary::from_legacy(user_id, email, username));
            }
            UserEvent::RegisteredV2 { user_id, email, first_name, last_name } => {
                ctx.upsert(UserSummary::from_names(user_id, email, first_name, last_name));
            }
            UserEvent::RegisteredV3 { user_id, email, profile } => {
                ctx.upsert(UserSummary::from_profile(user_id, email, profile));
            }
        }
        Ok(())
    }

    fn name(&self) -> &str {
        "user-list"
    }

    // `on_error(&mut self, ctx: FailureContext<'_, Self::Error>) ->
    // FailureStrategy` has a default impl (FailureStrategy::Fatal), so it is
    // omitted here. Override it to skip or retry specific failures.
}
```

The projector is then driven by `run_projection(projector, &backend, config)`.
See [Projections](../02-getting-started/04-projections.md) for the full runner
and `ProjectionConfig` details.

## Command Evolution

Commands evolve more freely than events because they are _never_ persisted —
they exist only at the moment of execution and carry no historical
compatibility constraint. You can add, remove, or restructure command fields
without affecting any stored data.

The example below uses ordinary Rust constructors and a builder. These are
**application-level conveniences**, not EventCore APIs — `#[derive(Command)]`
only generates the `CommandStreams` implementation (it does not generate a
builder):

```rust
use eventcore::Command;

// Application-level command with several fields.
#[derive(Command, Clone)]
struct CreateUser {
    #[stream]
    user_stream: StreamId,
    email: Email,
    first_name: FirstName,
    last_name: LastName,
    initial_preferences: UserPreferences,
    referral_code: Option<ReferralCode>,
}

// Application-level convenience constructors / builder (NOT generated by EventCore).
impl CreateUser {
    pub fn builder() -> CreateUserBuilder {
        CreateUserBuilder::default()
    }

    pub fn with_name(
        user_stream: StreamId,
        email: Email,
        first_name: FirstName,
        last_name: LastName,
    ) -> Self {
        Self {
            user_stream,
            email,
            first_name,
            last_name,
            initial_preferences: UserPreferences::default(),
            referral_code: None,
        }
    }
}

#[derive(Default)]
pub struct CreateUserBuilder {
    user_stream: Option<StreamId>,
    email: Option<Email>,
    first_name: Option<FirstName>,
    last_name: Option<LastName>,
    initial_preferences: Option<UserPreferences>,
    referral_code: Option<ReferralCode>,
}

impl CreateUserBuilder {
    pub fn email(mut self, email: Email) -> Self {
        self.email = Some(email);
        self
    }

    pub fn name(mut self, first: FirstName, last: LastName) -> Self {
        self.first_name = Some(first);
        self.last_name = Some(last);
        self
    }

    pub fn build(self) -> Result<CreateUser, BuildError> {
        Ok(CreateUser {
            user_stream: self.user_stream.ok_or(BuildError::MissingStream)?,
            email: self.email.ok_or(BuildError::MissingEmail)?,
            first_name: self.first_name.unwrap_or_default(),
            last_name: self.last_name.unwrap_or_default(),
            initial_preferences: self.initial_preferences.unwrap_or_default(),
            referral_code: self.referral_code,
        })
    }
}
```

Each `CreateUser` is executed through `execute(store, command, policy)`. Its
`handle()` emits only the latest event variant, so command evolution never
introduces old shapes into the store.

## Migration Strategies

### Forward-Only Evolution (Preferred)

The simplest approach — only add fields with serde defaults, never remove or
retype them. This keeps a single event variant indefinitely:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProductCreated {
    product_id: ProductId,
    name: ProductName,
    price: Money,

    // Later additions, all backwards-compatible via serde defaults
    #[serde(default)]
    category: Option<Category>,
    #[serde(default)]
    tags: Vec<Tag>,
    #[serde(default)]
    metadata: ProductMetadata,
    #[serde(default)]
    variants: Vec<ProductVariant>,
    #[serde(default = "default_status")]
    status: ProductStatus,
}

fn default_status() -> ProductStatus {
    ProductStatus::Active
}
```

Reach for this whenever the change is purely additive. You only need new
variants when a change is genuinely incompatible.

### Event Splitting

When a single monolithic event grows too broad, you can stop emitting it and
start emitting several focused events instead. The old monolithic variant
remains in the enum so historical events still replay; new commands emit the
focused variants:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
enum OrderEvent {
    // Original monolithic variant - kept for historical replay
    Processed {
        order_id: OrderId,
        payment_method: PaymentMethod,
        payment_amount: Money,
        shipping_address: Address,
        items: Vec<OrderItem>,
    },

    // Newer, focused variants emitted going forward
    PaymentProcessed { order_id: OrderId, payment_method: PaymentMethod, amount: Money },
    ShippingAddressSet { order_id: OrderId, address: Address },
    ItemsAdded { order_id: OrderId, items: Vec<OrderItem> },
}
```

`apply` and projectors handle both the old `Processed` variant and the new
focused ones.

## Testing Schema Evolution

The most important schema-evolution test is simply that **old JSON still
deserializes into the current event enum**. Because evolution is just serde,
you can assert this directly with `serde_json`.

### Backward-Compatibility Tests

```rust
#[cfg(test)]
mod schema_tests {
    use super::*;

    #[test]
    fn old_additive_event_deserializes_with_defaults() {
        // JSON written before `currency` and `tags` were added.
        let legacy = r#"{
            "ProductCreated": {
                "product_id": "p-1",
                "name": "Widget",
                "price": { "cents": 1000 }
            }
        }"#;

        let event: ProductEvent = serde_json::from_str(legacy).unwrap();
        match event {
            ProductEvent::ProductCreated { tags, status, .. } => {
                assert!(tags.is_empty());            // serde default
                assert_eq!(status, ProductStatus::Active); // default fn
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn historical_variant_still_deserializes() {
        // JSON for the original `Registered` variant, written before V2/V3 existed.
        let legacy = r#"{
            "Registered": {
                "user_id": "550e8400-e29b-41d4-a716-446655440000",
                "email": "legacy@example.com",
                "username": "legacy_user"
            }
        }"#;

        let event: UserEvent = serde_json::from_str(legacy).unwrap();
        assert!(matches!(event, UserEvent::Registered { .. }));
    }
}
```

### End-to-End Replay Tests

The strongest guarantee is an integration test that appends historical-shape
events and then runs a command (via `execute`) or a projection (via
`run_projection`) over them, asserting the observable outcome. Because old and
new variants live in the same enum, a single replay exercises every version of
`apply` (in the command) and `apply` (in the projector). See
[Testing](../02-getting-started/05-testing.md) for the harness and in-memory
store setup.

## Best Practices

1. **Prefer additive changes** — add fields with `#[serde(default)]` and keep a
   single variant whenever possible.
2. **Use `Option<T>` for genuine absence** — when a value was unknown at record
   time, model it as `Option`, not a sentinel or replay-time default.
3. **Add a new variant for incompatible changes** — never retype or remove a
   field on an existing variant.
4. **Never delete old variants** — they represent historical facts and must
   keep deserializing.
5. **Handle every variant** in `apply` and in projectors — let exhaustive
   matching catch omissions.
6. **Emit only the latest variant** from `handle`.
7. **Test old JSON against the current enum** — deserialization round-trips are
   the cheapest, highest-value evolution test.

## Why No Upcasting Registry

[ADR-0035](../../adr/ADR-0035-event-schema-evolution-via-enum-variants.md)
explicitly considered and **rejected** an upcasting system (a registry of
`fn(Value) -> Value` transformations applied at read time). The reasons:

- It adds a whole subsystem (version storage, an upcast registry, chain
  application) for a problem serde already solves.
- It transforms historical events at read time, eroding the "events are
  immutable facts" guarantee.
- It would require version metadata stored alongside every event, changing the
  storage schema across all backends.
- Enum variants achieve the same result with zero infrastructure.

So EventCore ships **no** `SchemaRegistry`, no migration trait, no
`VersionedEvent`, and no version metadata column for routing. Schema evolution
is plain serde plus Rust enums. If your application wants a higher-level
abstraction (for example, a helper that converts a stored variant into a
canonical in-memory shape), that is ordinary application code you own — it is
not part of EventCore's API.

## Summary

Schema evolution in EventCore:

- ✅ **Backward compatible** — old events still deserialize
- ✅ **Built on serde + enums** — no bespoke versioning infrastructure
- ✅ **Immutable** — historical events are never transformed
- ✅ **Type-safe** — exhaustive matching enforces handling every version
- ✅ **Testable** — old JSON round-trips against the current enum

Key patterns:

1. Use serde defaults for backwards-compatible additive changes.
2. Add a new enum variant for incompatible changes; keep the old one forever.
3. Handle all variants in `apply` and in projectors; emit only the latest from
   `handle`.
4. Test that historical JSON still deserializes.
5. Prefer additive evolution from day one.

Next, let's explore [Event Versioning](./02-event-versioning.md) →
