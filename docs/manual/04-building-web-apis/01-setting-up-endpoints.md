# Chapter 4.1: Setting Up HTTP Endpoints

EventCore is framework-agnostic - you can use it with any Rust web framework. This chapter shows how to integrate EventCore with popular frameworks and structure your API.

## Architecture Overview

```
HTTP Request → Web Framework → Command/Query → EventCore → Response
```

Your web layer should be thin, focusing on:

1. **Request parsing** - Convert HTTP to domain types
2. **Authentication** - Verify caller identity
3. **Authorization** - Check permissions
4. **Command/Query execution** - Delegate to EventCore
5. **Response formatting** - Convert results to HTTP

## Axum Integration

Axum is a modern web framework that pairs well with EventCore:

### Setup

```toml
[dependencies]
eventcore = "1.0"
axum = "0.7"
tokio = { version = "1", features = ["full"] }
tower = "0.4"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

### Basic Application Structure

```rust
use axum::{
    extract::{State, Json},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use eventcore::{execute, RetryPolicy};
use eventcore::postgres::PostgresEventStore;

// Application state shared across handlers.
//
// `PostgresEventStore` is `Clone` (it holds a connection pool internally), so
// it can be stored directly in axum state and cloned per request.
#[derive(Clone)]
struct AppState {
    event_store: PostgresEventStore,
}

#[tokio::main]
async fn main() {
    // Initialize the EventCore PostgreSQL backend and run migrations.
    let event_store = PostgresEventStore::new(
        "postgresql://localhost/eventcore"
    ).await.unwrap();
    event_store.migrate().await;

    let state = AppState {
        event_store,
    };

    // Build routes
    let app = Router::new()
        .route("/api/v1/tasks", post(create_task))
        .route("/api/v1/tasks/:id", get(get_task))
        .route("/api/v1/tasks/:id/assign", post(assign_task))
        .route("/api/v1/tasks/:id/complete", post(complete_task))
        .route("/api/v1/users/:id/tasks", get(get_user_tasks))
        .route("/health", get(health_check))
        .with_state(state);

    // Start server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .unwrap();

    axum::serve(listener, app).await.unwrap();
}
```

> **Read models / projections.** This example wires only the write path
> (`execute`). Read models are a separate code path in EventCore: a background
> task drives `eventcore::run_projection(projector, &backend, config)` to keep a
> query store up to date, and your `get_*` handlers read from that query store.
> Projections are not held inside the HTTP `AppState`; see
> [Projections](../02-getting-started/04-projections.md) and the projection
> runner (`ProjectionConfig`, `Projector`) for the real API.

### Command Handler Example

```rust
#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    title: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct CreateTaskResponse {
    task_id: String,
    message: String,
}

async fn create_task(
    State(state): State<AppState>,
    Json(request): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, ApiError> {
    // Validate input
    let title = TaskTitle::try_new(request.title)
        .map_err(|e| ApiError::validation(e))?;
    let description = TaskDescription::try_new(request.description)
        .map_err(|e| ApiError::validation(e))?;

    // Create command. `StreamId` is a validated newtype, so construct it with
    // `try_new` (the only constructor) and surface any validation failure.
    let task_id = TaskId::new();
    let stream_id = StreamId::try_new(format!("task-{}", task_id))
        .map_err(|e| ApiError::validation(e))?;
    let command = CreateTask {
        task_id: stream_id,
        title,
        description,
    };

    // Execute command. `execute` takes the store and command by value;
    // `PostgresEventStore` is `Clone`, so clone the pooled handle per request.
    execute(state.event_store.clone(), command, RetryPolicy::new())
        .await
        .map_err(|e| ApiError::from_command_error(e))?;

    // Return response
    Ok(Json(CreateTaskResponse {
        task_id: task_id.to_string(),
        message: "Task created successfully".to_string(),
    }))
}
```

### Error Handling

```rust
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
    details: Option<serde_json::Value>,
}

impl ApiError {
    fn validation<E: std::error::Error>(error: E) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
            details: None,
        }
    }

    fn from_command_error(error: CommandError) -> Self {
        match error {
            CommandError::ValidationError(msg) => Self {
                status: StatusCode::BAD_REQUEST,
                message: msg,
                details: None,
            },
            CommandError::BusinessRuleViolation(err) => Self {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                message: err.to_string(),
                details: None,
            },
            CommandError::ConcurrencyError(_) => Self {
                status: StatusCode::CONFLICT,
                message: "Resource was modified by another request".to_string(),
                details: None,
            },
            CommandError::EventStoreError(_) => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "An internal error occurred".to_string(),
                details: None,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({
            "error": {
                "message": self.message,
                "details": self.details,
            }
        });

        (self.status, Json(body)).into_response()
    }
}
```

## Actix Web Integration

Actix Web offers high performance and actor-based architecture:

### Setup

```toml
[dependencies]
eventcore = "1.0"
actix-web = "4"
actix-rt = "2"
```

### Application Structure

```rust
use actix_web::{web, App, HttpServer, HttpResponse, Result};
use eventcore::{execute, RetryPolicy};

struct AppData {
    event_store: PostgresEventStore,
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let event_store = PostgresEventStore::new(
        "postgresql://localhost/eventcore"
    ).await.unwrap();

    let app_data = web::Data::new(AppData {
        event_store,
    });

    HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .service(
                web::scope("/api/v1")
                    .route("/tasks", web::post().to(create_task))
                    .route("/tasks/{id}", web::get().to(get_task))
                    .route("/tasks/{id}/assign", web::post().to(assign_task))
            )
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}

async fn create_task(
    data: web::Data<AppData>,
    request: web::Json<CreateTaskRequest>,
) -> Result<HttpResponse> {
    // Similar to Axum example
    Ok(HttpResponse::Created().json(CreateTaskResponse {
        task_id: "...",
        message: "...",
    }))
}
```

## Rocket Integration

Rocket provides a declarative, type-safe approach:

### Setup

```toml
[dependencies]
eventcore = "1.0"
rocket = { version = "0.5", features = ["json"] }
```

### Application Structure

```rust
use rocket::{State, serde::json::Json};
use eventcore::{execute, RetryPolicy};

struct AppState {
    event_store: PostgresEventStore,
}

#[rocket::post("/tasks", data = "<request>")]
async fn create_task(
    state: &State<AppState>,
    request: Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, ApiError> {
    // Implementation similar to Axum
}

#[rocket::launch]
fn rocket() -> _ {
    let event_store = /* initialize */;

    rocket::build()
        .manage(AppState {
            event_store,
        })
        .mount("/api/v1", rocket::routes![
            create_task,
            get_task,
            assign_task,
        ])
}
```

## Request/Response Design

### Command Requests

Design your API requests to map cleanly to commands:

```rust
// HTTP Request
#[derive(Deserialize)]
struct TransferMoneyRequest {
    from_account: String,
    to_account: String,
    amount: Decimal,
    reference: Option<String>,
}

// Convert to command
impl TryFrom<TransferMoneyRequest> for TransferMoney {
    type Error = ValidationError;

    fn try_from(req: TransferMoneyRequest) -> Result<Self, Self::Error> {
        Ok(TransferMoney {
            from_account: StreamId::try_new(req.from_account)?,
            to_account: StreamId::try_new(req.to_account)?,
            amount: Money::try_from_decimal(req.amount)?,
            reference: req.reference.unwrap_or_default(),
        })
    }
}
```

### Response Design

Return minimal, useful information:

```rust
#[derive(Serialize)]
#[serde(tag = "status")]
enum CommandResponse {
    #[serde(rename = "success")]
    Success {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        resource_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resource_url: Option<String>,
    },
    #[serde(rename = "accepted")]
    Accepted {
        message: String,
        tracking_id: String,
    },
}
```

## Middleware and Interceptors

### Request ID Middleware

Track requests through your system:

```rust
use axum::middleware::{self, Next};
use axum::extract::Request;
use uuid::Uuid;

async fn request_id_middleware(
    mut request: Request,
    next: Next,
) -> impl IntoResponse {
    let request_id = Uuid::new_v4().to_string();

    // Add to request extensions
    request.extensions_mut().insert(RequestId(request_id.clone()));

    // Add to response headers
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "X-Request-ID",
        request_id.parse().unwrap(),
    );

    response
}

// Use in router
let app = Router::new()
    .route("/api/v1/tasks", post(create_task))
    .layer(middleware::from_fn(request_id_middleware));
```

### Timing Middleware

Monitor performance:

```rust
use std::time::Instant;

async fn timing_middleware(
    request: Request,
    next: Next,
) -> impl IntoResponse {
    let start = Instant::now();
    let path = request.uri().path().to_owned();
    let method = request.method().clone();

    let response = next.run(request).await;

    let duration = start.elapsed();
    tracing::info!(
        method = %method,
        path = %path,
        duration_ms = %duration.as_millis(),
        status = %response.status(),
        "Request completed"
    );

    response
}
```

## Configuration

> **EventCore reads no environment variables and ships no config loader.** All
> application configuration — ports, hosts, the database URL — is owned by your
> application. The struct below is illustrative application-level code (using the
> third-party `config` crate); it is not part of EventCore's API. You feed the
> resulting values into the EventCore backend constructors yourself.

A typical application defines its own configuration struct and loads it however
it likes (environment variables, a config file, CLI flags):

```rust
// Application-level configuration — NOT an EventCore type.
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default = "default_port")]
    port: u16,

    #[serde(default = "default_host")]
    host: String,

    database_url: String,

    #[serde(default = "default_max_connections")]
    max_connections: u32,
}

fn default_port() -> u16 { 3000 }
fn default_host() -> String { "0.0.0.0".to_string() }
fn default_max_connections() -> u32 { 20 }

impl Config {
    fn from_env() -> Result<Self, config::ConfigError> {
        let mut cfg = config::Config::default();

        // Load from environment
        cfg.merge(config::Environment::default())?;

        // Load from file if exists
        if std::path::Path::new("config.toml").exists() {
            cfg.merge(config::File::with_name("config"))?;
        }

        cfg.try_into()
    }
}
```

### Configuring the EventCore backend

To tune the PostgreSQL backend, build a `PostgresConfig` from your application
config and pass it to `PostgresEventStore::with_config`. `PostgresConfig`
exposes three fields — `max_connections` (a `MaxConnections` newtype wrapping a
`NonZeroU32`), `acquire_timeout`, and `idle_timeout` — and defaults to 10
connections, a 30-second acquire timeout, and a 10-minute idle timeout:

```rust
use std::num::NonZeroU32;
use std::time::Duration;
use eventcore::postgres::{MaxConnections, PostgresConfig, PostgresEventStore};

let config = Config::from_env().unwrap();

let max_connections = NonZeroU32::new(config.max_connections)
    .unwrap_or(NonZeroU32::new(10).expect("10 is non-zero"));

let pg_config = PostgresConfig {
    max_connections: MaxConnections::new(max_connections),
    acquire_timeout: Duration::from_secs(30),
    idle_timeout: Duration::from_secs(600),
};

let event_store =
    PostgresEventStore::with_config(config.database_url, pg_config)
        .await
        .unwrap();
event_store.migrate().await;
```

The SQLite backend is configured analogously with `SqliteConfig` passed to
`SqliteEventStore::new`. There is no global `EventCoreConfig`, `ConfigBuilder`,
or environment-variable convention — each backend is constructed explicitly.

## Health Checks

Expose system health from your own probe logic.

> EventCore does **not** provide a generic `health_check()` method on the event
> store. The PostgreSQL backend offers `PostgresEventStore::ping()`, but it
> returns `()` and **panics** if the database is unreachable — it is intended as
> a fail-fast startup check, not a graceful liveness probe. For an HTTP health
> endpoint that reports degraded/unhealthy instead of crashing, write your own
> probe that returns a `Result`. The `HealthResponse` / `HealthStatus` /
> `CheckResult` types below are application-level, not EventCore APIs.

A simple approach is to run a trivial query through your own pool handle (or any
read your application already owns) and map success/failure to a check result:

```rust
// Application-level health types — NOT EventCore APIs.
#[derive(Serialize)]
struct HealthResponse {
    status: HealthStatus,
    version: &'static str,
    checks: HashMap<String, CheckResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

async fn health_check(State(state): State<AppState>) -> Json<HealthResponse> {
    let mut checks = HashMap::new();

    // Check the event store with an application-owned probe that returns a
    // Result rather than panicking. `probe_event_store` is your own function —
    // for example, issuing a lightweight read and catching the error.
    match probe_event_store(&state.event_store).await {
        Ok(_) => checks.insert("event_store".to_string(), CheckResult::healthy()),
        Err(e) => checks.insert("event_store".to_string(), CheckResult::unhealthy(e)),
    };

    // Projection health is tracked by your own projection-runner supervision
    // (the task that drives `eventcore::run_projection`), not by an EventCore
    // type held in `AppState`. Report whatever liveness your supervisor exposes.

    // Overall status
    let status = if checks.values().all(|c| c.is_healthy()) {
        HealthStatus::Healthy
    } else if checks.values().any(|c| c.is_unhealthy()) {
        HealthStatus::Unhealthy
    } else {
        HealthStatus::Degraded
    };

    Json(HealthResponse {
        status,
        version: env!("CARGO_PKG_VERSION"),
        checks,
    })
}
```

## Graceful Shutdown

Handle shutdown gracefully:

```rust
use tokio::signal;

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// In main
let app = /* build app */;

axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
```

## Testing HTTP Endpoints

Test your API endpoints:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_create_task_success() {
        let app = create_test_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{
                        "title": "Test Task",
                        "description": "Test Description"
                    }"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);

        let body: CreateTaskResponse = serde_json::from_slice(
            &hyper::body::to_bytes(response.into_body()).await.unwrap()
        ).unwrap();

        assert!(!body.task_id.is_empty());
    }

    async fn create_test_app() -> Router {
        // `InMemoryEventStore` (from the `eventcore-memory` crate) is the
        // zero-dependency backend for tests. To swap it in here, make `AppState`
        // generic over the store type (`AppState<S: EventStore>`) so the same
        // handlers work against `PostgresEventStore` in production and
        // `InMemoryEventStore` in tests. `execute` is generic over any
        // `EventStore`, so the handler code does not change.
        let event_store = InMemoryEventStore::new();
        let state = AppState { event_store };

        Router::new()
            .route("/api/v1/tasks", post(create_task))
            .with_state(state)
    }
}
```

## Best Practices

1. **Keep handlers thin** - Delegate business logic to commands
2. **Use proper status codes** - 201 for creation, 202 for accepted, etc.
3. **Version your API** - Use URL versioning (/api/v1/)
4. **Document with OpenAPI** - Generate from code when possible
5. **Use correlation IDs** - Track requests across services
6. **Log appropriately** - Info for requests, error for failures
7. **Handle errors gracefully** - Never expose internal details

## Summary

Setting up HTTP endpoints for EventCore:

- ✅ **Framework agnostic** - Works with any Rust web framework
- ✅ **Thin HTTP layer** - Focus on translation, not business logic
- ✅ **Type-safe** - Leverage Rust's type system
- ✅ **Error handling** - Map domain errors to HTTP responses
- ✅ **Testable** - Easy to test endpoints in isolation

Key patterns:

1. Parse and validate requests early
2. Convert to domain commands
3. Execute with EventCore
4. Map results to HTTP responses
5. Handle errors appropriately

Next, let's explore [Command Handlers](./02-command-handlers.md) →
