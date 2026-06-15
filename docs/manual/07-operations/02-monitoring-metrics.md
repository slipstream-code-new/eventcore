# Chapter 6.2: Monitoring and Metrics

Effective monitoring is crucial for operating EventCore applications in production. This chapter covers comprehensive observability strategies including metrics, logging, tracing, and alerting.

> **What EventCore provides vs. what your application owns.** EventCore does
> not ship a metrics registry, a Prometheus exporter, a logging configuration,
> or a CLI. There is no `eventcore::monitoring` module and EventCore reads no
> environment variables. Instead, EventCore gives you two integration points
> and stays out of your way:
>
> - **`tracing` spans.** `execute()` is annotated with `#[tracing::instrument]`,
>   so command execution already emits spans you can subscribe to with any
>   `tracing-subscriber` layer.
> - **The `MetricsHook` trait.** Implement it and attach it with
>   `RetryPolicy::with_metrics_hook(...)` to observe the retry lifecycle
>   (which streams retried, the attempt number, and the backoff delay).
>
> Everything else in this chapter — Prometheus counters, log shipping,
> OpenTelemetry wiring, dashboards, alert rules — is **application-level code
> you write around EventCore**. The examples below are illustrative scaffolding
> for your service, not APIs exported by the library.

## Metrics Collection

### Prometheus Integration (application-level)

EventCore has no built-in Prometheus support, but it is straightforward to
define your own metrics in the [`prometheus`](https://crates.io/crates/prometheus)
crate and update them from the code that wraps `eventcore::execute()`. The
following `MetricsService` is **application code** — adapt it to your service:

```rust
use prometheus::{
    Counter, Histogram, Gauge, IntGauge,
    register_counter, register_histogram, register_gauge, register_int_gauge,
    Encoder, TextEncoder
};
use axum::{response::Response, http::StatusCode};

lazy_static! {
    // Command execution metrics
    static ref COMMANDS_TOTAL: Counter = register_counter!(
        "eventcore_commands_total",
        "Total number of commands executed"
    ).unwrap();

    static ref COMMAND_DURATION: Histogram = register_histogram!(
        "eventcore_command_duration_seconds",
        "Command execution duration in seconds"
    ).unwrap();

    static ref COMMAND_ERRORS: Counter = register_counter!(
        "eventcore_command_errors_total",
        "Total number of command execution errors"
    ).unwrap();

    // Number of attempts a command needed before committing, as reported by
    // ExecutionResponse::attempts() (1 = first-try success, >1 = retries).
    static ref COMMAND_ATTEMPTS: Histogram = register_histogram!(
        "eventcore_command_attempts",
        "Execution attempts per command (1 = no retries)"
    ).unwrap();

    // Event store metrics
    static ref EVENTS_WRITTEN: Counter = register_counter!(
        "eventcore_events_written_total",
        "Total number of events written to the store"
    ).unwrap();

    static ref EVENT_STORE_LATENCY: Histogram = register_histogram!(
        "eventcore_event_store_latency_seconds",
        "Event store operation latency in seconds"
    ).unwrap();

    // Stream metrics
    static ref ACTIVE_STREAMS: IntGauge = register_int_gauge!(
        "eventcore_active_streams",
        "Number of active event streams"
    ).unwrap();

    static ref STREAM_VERSIONS: Gauge = register_gauge!(
        "eventcore_stream_versions",
        "Current version of event streams"
    ).unwrap();

    // Projection metrics
    static ref PROJECTION_EVENTS_PROCESSED: Counter = register_counter!(
        "eventcore_projection_events_processed_total",
        "Total events processed by projections"
    ).unwrap();

    static ref PROJECTION_LAG: Gauge = register_gauge!(
        "eventcore_projection_lag_seconds",
        "Projection lag behind latest events in seconds"
    ).unwrap();

    // System metrics
    static ref MEMORY_USAGE: Gauge = register_gauge!(
        "eventcore_memory_usage_bytes",
        "Memory usage in bytes"
    ).unwrap();

    static ref CONNECTION_POOL_SIZE: IntGauge = register_int_gauge!(
        "eventcore_connection_pool_size",
        "Database connection pool size"
    ).unwrap();
}

#[derive(Clone)]
pub struct MetricsService {
    start_time: std::time::Instant,
}

impl MetricsService {
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
        }
    }

    pub fn record_command_executed(&self, command_type: &str, duration: std::time::Duration, success: bool) {
        COMMANDS_TOTAL.with_label_values(&[command_type]).inc();
        COMMAND_DURATION.with_label_values(&[command_type]).observe(duration.as_secs_f64());

        if !success {
            COMMAND_ERRORS.with_label_values(&[command_type]).inc();
        }
    }

    pub fn record_command_attempts(&self, command_type: &str, attempts: u32) {
        // ExecutionResponse::attempts() is the single piece of execution
        // metadata EventCore returns; record it for retry observability.
        COMMAND_ATTEMPTS
            .with_label_values(&[command_type])
            .observe(f64::from(attempts));
    }

    pub fn record_events_written(&self, stream_id: &str, count: usize) {
        EVENTS_WRITTEN.with_label_values(&[stream_id]).inc_by(count as f64);
    }

    pub fn record_event_store_operation(&self, operation: &str, duration: std::time::Duration) {
        EVENT_STORE_LATENCY.with_label_values(&[operation]).observe(duration.as_secs_f64());
    }

    pub fn update_active_streams(&self, count: i64) {
        ACTIVE_STREAMS.set(count);
    }

    pub fn update_stream_version(&self, stream_id: &str, version: f64) {
        STREAM_VERSIONS.with_label_values(&[stream_id]).set(version);
    }

    pub fn record_projection_event(&self, projection_name: &str, lag_seconds: f64) {
        PROJECTION_EVENTS_PROCESSED.with_label_values(&[projection_name]).inc();
        PROJECTION_LAG.with_label_values(&[projection_name]).set(lag_seconds);
    }

    pub fn update_memory_usage(&self, bytes: f64) {
        MEMORY_USAGE.set(bytes);
    }

    pub fn update_connection_pool_size(&self, size: i64) {
        CONNECTION_POOL_SIZE.set(size);
    }

    pub async fn export_metrics(&self) -> Result<Response<String>, StatusCode> {
        let encoder = TextEncoder::new();
        let metric_families = prometheus::gather();

        match encoder.encode_to_string(&metric_families) {
            Ok(output) => {
                let response = Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", encoder.format_type())
                    .body(output)
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                Ok(response)
            }
            Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

// Metrics endpoint handler
pub async fn metrics_handler(
    State(metrics_service): State<MetricsService>
) -> Result<Response<String>, StatusCode> {
    metrics_service.export_metrics().await
}
```

### Custom Metrics (application-level)

Define application-specific metrics the same way:

```rust
use prometheus::{register_counter_vec, register_histogram_vec, CounterVec, HistogramVec};

lazy_static! {
    // Business metrics
    static ref USER_REGISTRATIONS: Counter = register_counter!(
        "eventcore_user_registrations_total",
        "Total number of user registrations"
    ).unwrap();

    static ref ORDER_VALUE: Histogram = register_histogram!(
        "eventcore_order_value_dollars",
        "Order value in dollars",
        vec![10.0, 50.0, 100.0, 500.0, 1000.0, 5000.0]
    ).unwrap();

    static ref API_REQUESTS: CounterVec = register_counter_vec!(
        "eventcore_api_requests_total",
        "Total API requests",
        &["method", "endpoint", "status"]
    ).unwrap();

    static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "eventcore_request_duration_seconds",
        "Request duration in seconds",
        &["method", "endpoint"]
    ).unwrap();
}

pub struct BusinessMetrics;

impl BusinessMetrics {
    pub fn record_user_registration() {
        USER_REGISTRATIONS.inc();
    }

    pub fn record_order_placed(value_dollars: f64) {
        ORDER_VALUE.observe(value_dollars);
    }

    pub fn record_api_request(method: &str, endpoint: &str, status: u16, duration: std::time::Duration) {
        API_REQUESTS
            .with_label_values(&[method, endpoint, &status.to_string()])
            .inc();

        REQUEST_DURATION
            .with_label_values(&[method, endpoint])
            .observe(duration.as_secs_f64());
    }
}
```

### Automatic Instrumentation (application-level)

The cleanest way to instrument command execution is to wrap the free-function
`eventcore::execute()` API in a helper of your own. Note the real signature:
`execute()` takes the store and command **by value** (`store: S`, `command: C`),
so the store must be cheaply cloneable (the built-in stores are `Clone`):

```rust
use std::time::Instant;
use eventcore::{execute, CommandLogic, ExecutionResponse, RetryPolicy, CommandError};
use eventcore_types::EventStore; // EventStore is not re-exported from the eventcore facade

// Instrumented command execution wrapping the free-function execute() API.
pub async fn execute_instrumented<C, S>(
    store: S,
    command: C,
    policy: RetryPolicy,
    metrics: &MetricsService,
) -> Result<ExecutionResponse, CommandError>
where
    C: CommandLogic,
    S: EventStore,
{
    let start = Instant::now();
    let command_type = std::any::type_name::<C>();

    let result = execute(store, command, policy).await;
    let duration = start.elapsed();
    let success = result.is_ok();

    metrics.record_command_executed(command_type, duration, success);

    // On success, `ExecutionResponse::attempts()` reports how many tries the
    // command needed (1 = committed on the first attempt; >1 = optimistic
    // concurrency retries occurred). That is the only metadata it exposes.
    if let Ok(response) = &result {
        metrics.record_command_attempts(command_type, response.attempts());
    }

    result
}
```

For visibility into the **retry lifecycle specifically**, implement the
`MetricsHook` trait and attach it to your `RetryPolicy`. EventCore invokes the
hook before each retry attempt, passing a `RetryContext`:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use eventcore::{MetricsHook, RetryContext, RetryPolicy};

struct RetryMetricsHook {
    retries: AtomicU64,
}

impl MetricsHook for RetryMetricsHook {
    fn on_retry_attempt(&self, ctx: &RetryContext) {
        // RetryContext exposes the public fields:
        //   ctx.streams: Vec<StreamId>   (the streams being retried)
        //   ctx.attempt: AttemptNumber   (1-based attempt counter)
        //   ctx.delay_ms: DelayMilliseconds (backoff before this attempt)
        self.retries.fetch_add(1, Ordering::Relaxed);
        // Forward to your metrics backend, e.g. a Prometheus counter:
        // COMMAND_RETRIES.with_label_values(&[...]).inc();
    }
}

let policy = RetryPolicy::new()
    .max_retries(2)
    .with_metrics_hook(RetryMetricsHook { retries: AtomicU64::new(0) });
```

## Structured Logging

### Logging Configuration

```rust
use tracing::{info, warn, error, debug, trace, instrument};
use tracing_subscriber::{
    layer::SubscriberExt,
    util::SubscriberInitExt,
    fmt,
    EnvFilter,
};
use serde_json::json;

pub fn init_logging(log_level: &str, log_format: &str) -> Result<(), Box<dyn std::error::Error>> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(log_level));

    let fmt_layer = match log_format {
        "json" => {
            fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .boxed()
        }
        _ => {
            fmt::layer()
                .with_target(true)
                .with_file(true)
                .with_line_number(true)
                .boxed()
        }
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .init();

    Ok(())
}

// Structured logging for command execution (application-level wrapper).
//
// `execute()` takes the store and command by value, so the helper is generic
// over the store type `S`. `EventStore` lives in `eventcore_types` (it is not
// re-exported from the `eventcore` facade).
#[instrument(skip(command, store), fields(command_type = %std::any::type_name::<C>()))]
pub async fn execute_command_with_logging<C, S>(
    command: C,
    store: S,
) -> Result<ExecutionResponse, CommandError>
where
    C: CommandLogic,
    S: eventcore_types::EventStore,
{
    debug!("Starting command execution");

    let result = execute(store, command, RetryPolicy::new()).await;

    match &result {
        Ok(response) => {
            // ExecutionResponse exposes a single accessor: attempts(). It has
            // no events or streams fields — EventCore does not surface the
            // written events from execute(). If you need the events a command
            // produced, read them back with the store's read_stream() API.
            info!(
                attempts = response.attempts(),
                "Command executed successfully"
            );
        }
        Err(error) => {
            // CommandError carries its source chain; `%error` renders its
            // Display message (a human-readable string). Variants:
            // BusinessRuleViolation (transparent — renders the wrapped command
            // error's own message), ConcurrencyError, EventStoreError, ValidationError.
            error!(
                error = %error,
                "Command execution failed"
            );
        }
    }

    result
}
```

> **There is no separate "write events" entry point to log.** Application code
> never appends events directly. A command's `handle()` returns `NewEvents` and
> `execute()` appends them atomically with optimistic-concurrency checks. The
> low-level `EventStore::append_events(StreamWrites)` /
> `read_stream(StreamId) -> EventStream` methods are the backend contract —
> they are exercised by `execute()`, not called from handlers. To observe the
> write path, instrument your `execute()` wrapper (above) or use the
> `MetricsHook` for retry visibility; there is no `EventToWrite`,
> `WriteResult`, or `write_events()` API.

### Log Aggregation

Configure log shipping to centralized systems:

```yaml
# Fluentd configuration for Kubernetes
apiVersion: v1
kind: ConfigMap
metadata:
  name: fluentd-config
  namespace: eventcore
data:
  fluent.conf: |
    <source>
      @type tail
      path /var/log/containers/eventcore-*.log
      pos_file /var/log/fluentd-eventcore.log.pos
      tag eventcore.*
      format json
      time_key time
      time_format %Y-%m-%dT%H:%M:%S.%NZ
    </source>

    <filter eventcore.**>
      @type parser
      key_name log
      format json
      reserve_data true
    </filter>

    <match eventcore.**>
      @type elasticsearch
      host elasticsearch.logging.svc.cluster.local
      port 9200
      index_name eventcore-logs
      type_name _doc
      include_timestamp true
      logstash_format true
      logstash_prefix eventcore

      <buffer>
        @type file
        path /var/log/fluentd-buffers/eventcore
        flush_mode interval
        retry_type exponential_backoff
        flush_thread_count 2
        flush_interval 5s
        retry_forever
        retry_max_interval 30
        chunk_limit_size 2M
        queue_limit_length 8
        overflow_action block
      </buffer>
    </match>
```

## Distributed Tracing

EventCore's `execute()` is annotated with `#[tracing::instrument]`, so it emits
a span named `execute` automatically. The wiring below is **application-level**
OpenTelemetry setup that subscribes to those spans (plus your own). EventCore
itself reads no environment variables — the `ENVIRONMENT` lookup below is your
service's own configuration, not something the library consumes.

### OpenTelemetry Integration (application-level)

```rust
use opentelemetry::{
    global,
    trace::{TraceError, Tracer, TracerProvider},
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{self, Sampler},
    Resource,
};
use tracing_opentelemetry::OpenTelemetryLayer;

pub fn init_tracing(service_name: &str, otlp_endpoint: &str) -> Result<(), TraceError> {
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(otlp_endpoint)
        )
        .with_trace_config(
            trace::config()
                .with_sampler(Sampler::TraceIdRatioBased(1.0))
                .with_resource(Resource::new(vec![
                    KeyValue::new("service.name", service_name.to_string()),
                    KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                    KeyValue::new("deployment.environment",
                        std::env::var("ENVIRONMENT").unwrap_or_else(|_| "unknown".to_string())
                    ),
                ]))
        )
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;

    let telemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(telemetry_layer)
        .init();

    Ok(())
}

// Traced command execution (application-level wrapper).
#[tracing::instrument(skip(command, store), fields(command_id = %uuid::Uuid::new_v4()))]
pub async fn execute_command_traced<C, S>(
    command: C,
    store: S,
) -> Result<ExecutionResponse, CommandError>
where
    C: CommandLogic,
    S: eventcore_types::EventStore,
{
    let span = tracing::Span::current();
    span.record("command.type", std::any::type_name::<C>());

    let result = execute(store, command, RetryPolicy::new()).await;

    match &result {
        Ok(response) => {
            span.record("command.success", true);
            // The only execution metadata available is the attempt count.
            // ExecutionResponse does not expose written events or streams.
            span.record("command.attempts", response.attempts());
        }
        Err(error) => {
            span.record("command.success", false);
            span.record("error.message", format!("{}", error));
            span.record("error.type", std::any::type_name_of_val(error));
        }
    }

    result
}

// Cross-service trace propagation
use axum::{
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

pub async fn trace_propagation_middleware(
    request: Request,
    next: Next,
) -> Response {
    // Extract trace context from headers
    let headers = request.headers();
    let parent_context = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderMapCarrier::new(headers))
    });

    // Create new span with parent context
    let span = tracing::info_span!(
        "http_request",
        method = %request.method(),
        uri = %request.uri(),
        version = ?request.version(),
    );

    // Set parent context
    span.set_parent(parent_context);

    // Execute request within span
    let response = span.in_scope(|| next.run(request)).await;

    response
}

struct HeaderMapCarrier<'a> {
    headers: &'a HeaderMap,
}

impl<'a> HeaderMapCarrier<'a> {
    fn new(headers: &'a HeaderMap) -> Self {
        Self { headers }
    }
}

impl<'a> opentelemetry::propagation::Extractor for HeaderMapCarrier<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        self.headers.get(key)?.to_str().ok()
    }

    fn keys(&self) -> Vec<&str> {
        self.headers.keys().map(|k| k.as_str()).collect()
    }
}
```

## Alerting

The alert rules below reference the `eventcore_*` Prometheus series **your
application** exposes (defined in the Metrics Collection section). They are not
emitted by the library itself — adjust the metric names to match whatever your
`MetricsService` registers.

### Prometheus Alerting Rules

```yaml
# prometheus-alerts.yaml
groups:
  - name: eventcore.rules
    rules:
      # High error rate
      - alert: HighCommandErrorRate
        expr: |
          (
            rate(eventcore_command_errors_total[5m]) /
            rate(eventcore_commands_total[5m])
          ) > 0.05
        for: 2m
        labels:
          severity: warning
          service: eventcore
        annotations:
          summary: "High command error rate detected"
          description: "Command error rate is {{ $value | humanizePercentage }} over the last 5 minutes"

      # High latency
      - alert: HighCommandLatency
        expr: |
          histogram_quantile(0.95, rate(eventcore_command_duration_seconds_bucket[5m])) > 1.0
        for: 3m
        labels:
          severity: warning
          service: eventcore
        annotations:
          summary: "High command latency detected"
          description: "95th percentile command latency is {{ $value }}s"

      # Event store issues
      - alert: EventStoreDown
        expr: up{job="eventcore"} == 0
        for: 1m
        labels:
          severity: critical
          service: eventcore
        annotations:
          summary: "EventCore service is down"
          description: "EventCore service has been down for more than 1 minute"

      # Projection lag
      - alert: ProjectionLag
        expr: eventcore_projection_lag_seconds > 300
        for: 5m
        labels:
          severity: warning
          service: eventcore
        annotations:
          summary: "Projection lag is high"
          description: "Projection {{ $labels.projection_name }} is {{ $value }}s behind"

      # Memory usage
      - alert: HighMemoryUsage
        expr: |
          (eventcore_memory_usage_bytes / (1024 * 1024 * 1024)) > 1.0
        for: 5m
        labels:
          severity: warning
          service: eventcore
        annotations:
          summary: "High memory usage"
          description: "Memory usage is {{ $value | humanize }}GB"

      # Database connection pool
      - alert: DatabaseConnectionPoolExhausted
        expr: eventcore_connection_pool_size / eventcore_connection_pool_max_size > 0.9
        for: 2m
        labels:
          severity: critical
          service: eventcore
        annotations:
          summary: "Database connection pool nearly exhausted"
          description: "Connection pool utilization is {{ $value | humanizePercentage }}"
```

### Alert Manager Configuration

```yaml
# alertmanager.yaml
global:
  smtp_smarthost: "smtp.example.com:587"
  smtp_from: "alerts@eventcore.com"

route:
  group_by: ["alertname", "service"]
  group_wait: 10s
  group_interval: 10s
  repeat_interval: 1h
  receiver: "web.hook"
  routes:
    - match:
        severity: critical
      receiver: "critical-alerts"
    - match:
        severity: warning
      receiver: "warning-alerts"

receivers:
  - name: "web.hook"
    webhook_configs:
      - url: "http://slack-webhook/webhook"

  - name: "critical-alerts"
    email_configs:
      - to: "oncall@eventcore.com"
        subject: "CRITICAL: {{ range .Alerts }}{{ .Annotations.summary }}{{ end }}"
        body: |
          {{ range .Alerts }}
          Alert: {{ .Annotations.summary }}
          Description: {{ .Annotations.description }}
          Labels: {{ range .Labels.SortedPairs }}{{ .Name }}={{ .Value }} {{ end }}
          {{ end }}
    slack_configs:
      - api_url: "https://hooks.slack.com/services/YOUR/SLACK/WEBHOOK"
        channel: "#critical-alerts"
        title: "Critical Alert: {{ range .Alerts }}{{ .Annotations.summary }}{{ end }}"

  - name: "warning-alerts"
    slack_configs:
      - api_url: "https://hooks.slack.com/services/YOUR/SLACK/WEBHOOK"
        channel: "#warnings"
        title: "Warning: {{ range .Alerts }}{{ .Annotations.summary }}{{ end }}"
```

## Grafana Dashboards

The dashboard JSON below queries the same application-defined `eventcore_*`
metrics. Tailor the panel queries to the series your service actually exports.

### EventCore Operations Dashboard

```json
{
  "dashboard": {
    "title": "EventCore Operations",
    "panels": [
      {
        "title": "Command Execution Rate",
        "type": "graph",
        "targets": [
          {
            "expr": "rate(eventcore_commands_total[5m])",
            "legendFormat": "Commands/sec"
          }
        ]
      },
      {
        "title": "Command Latency",
        "type": "graph",
        "targets": [
          {
            "expr": "histogram_quantile(0.50, rate(eventcore_command_duration_seconds_bucket[5m]))",
            "legendFormat": "p50"
          },
          {
            "expr": "histogram_quantile(0.95, rate(eventcore_command_duration_seconds_bucket[5m]))",
            "legendFormat": "p95"
          },
          {
            "expr": "histogram_quantile(0.99, rate(eventcore_command_duration_seconds_bucket[5m]))",
            "legendFormat": "p99"
          }
        ]
      },
      {
        "title": "Error Rate",
        "type": "singlestat",
        "targets": [
          {
            "expr": "rate(eventcore_command_errors_total[5m]) / rate(eventcore_commands_total[5m])",
            "legendFormat": "Error Rate"
          }
        ],
        "thresholds": [
          {
            "value": 0.01,
            "colorMode": "critical"
          }
        ]
      },
      {
        "title": "Active Streams",
        "type": "singlestat",
        "targets": [
          {
            "expr": "eventcore_active_streams",
            "legendFormat": "Streams"
          }
        ]
      },
      {
        "title": "Projection Lag",
        "type": "graph",
        "targets": [
          {
            "expr": "eventcore_projection_lag_seconds",
            "legendFormat": "{{ projection_name }}"
          }
        ]
      },
      {
        "title": "Memory Usage",
        "type": "graph",
        "targets": [
          {
            "expr": "eventcore_memory_usage_bytes / (1024 * 1024 * 1024)",
            "legendFormat": "Memory (GB)"
          }
        ]
      }
    ]
  }
}
```

## Performance Monitoring

### Real-Time Performance Metrics (application-level)

The `PerformanceMonitor` below is **application code** that aggregates the
Prometheus metrics your service defines (see the Metrics Collection section).
EventCore does not provide a `PerformanceMonitor`, `PerformanceSnapshot`, or any
metrics-aggregation type — this is a pattern you implement on top of your own
metrics backend.

```rust
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct PerformanceSnapshot {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub commands_per_second: f64,
    pub events_per_second: f64,
    pub avg_latency_ms: f64,
    pub p95_latency_ms: f64,
    pub p99_latency_ms: f64,
    pub error_rate: f64,
    pub active_streams: i64,
    pub memory_usage_mb: f64,
}

pub struct PerformanceMonitor {
    snapshots: Arc<RwLock<Vec<PerformanceSnapshot>>>,
    max_snapshots: usize,
}

impl PerformanceMonitor {
    pub fn new(max_snapshots: usize) -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(Vec::new())),
            max_snapshots,
        }
    }

    pub async fn capture_snapshot(&self) -> PerformanceSnapshot {
        let snapshot = PerformanceSnapshot {
            timestamp: chrono::Utc::now(),
            commands_per_second: self.calculate_command_rate().await,
            events_per_second: self.calculate_event_rate().await,
            avg_latency_ms: self.calculate_avg_latency().await,
            p95_latency_ms: self.calculate_p95_latency().await,
            p99_latency_ms: self.calculate_p99_latency().await,
            error_rate: self.calculate_error_rate().await,
            active_streams: self.get_active_stream_count().await,
            memory_usage_mb: self.get_memory_usage_mb().await,
        };

        let mut snapshots = self.snapshots.write().await;
        snapshots.push(snapshot.clone());

        // Keep only the most recent snapshots
        if snapshots.len() > self.max_snapshots {
            snapshots.remove(0);
        }

        snapshot
    }

    pub async fn get_trend_analysis(&self, minutes: u64) -> TrendAnalysis {
        let snapshots = self.snapshots.read().await;
        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(minutes as i64);

        let recent_snapshots: Vec<_> = snapshots
            .iter()
            .filter(|s| s.timestamp > cutoff)
            .collect();

        if recent_snapshots.is_empty() {
            return TrendAnalysis::default();
        }

        TrendAnalysis {
            throughput_trend: self.calculate_trend(&recent_snapshots, |s| s.commands_per_second),
            latency_trend: self.calculate_trend(&recent_snapshots, |s| s.avg_latency_ms),
            error_rate_trend: self.calculate_trend(&recent_snapshots, |s| s.error_rate),
            memory_trend: self.calculate_trend(&recent_snapshots, |s| s.memory_usage_mb),
        }
    }

    async fn calculate_command_rate(&self) -> f64 {
        // Get rate from Prometheus metrics
        // Implementation depends on your metrics backend
        0.0
    }

    async fn calculate_event_rate(&self) -> f64 {
        // Get rate from Prometheus metrics
        0.0
    }

    async fn calculate_avg_latency(&self) -> f64 {
        // Get average latency from metrics
        0.0
    }

    async fn calculate_p95_latency(&self) -> f64 {
        // Get p95 latency from metrics
        0.0
    }

    async fn calculate_p99_latency(&self) -> f64 {
        // Get p99 latency from metrics
        0.0
    }

    async fn calculate_error_rate(&self) -> f64 {
        // Calculate error rate from metrics
        0.0
    }

    async fn get_active_stream_count(&self) -> i64 {
        // Get active stream count from metrics
        0
    }

    async fn get_memory_usage_mb(&self) -> f64 {
        // Get memory usage from system metrics
        0.0
    }

    fn calculate_trend<F>(&self, snapshots: &[&PerformanceSnapshot], extractor: F) -> Trend
    where
        F: Fn(&PerformanceSnapshot) -> f64,
    {
        if snapshots.len() < 2 {
            return Trend::Stable;
        }

        let values: Vec<f64> = snapshots.iter().map(|s| extractor(s)).collect();
        let first_half = &values[0..values.len()/2];
        let second_half = &values[values.len()/2..];

        let first_avg = first_half.iter().sum::<f64>() / first_half.len() as f64;
        let second_avg = second_half.iter().sum::<f64>() / second_half.len() as f64;

        let change_percent = (second_avg - first_avg) / first_avg * 100.0;

        match change_percent {
            x if x > 10.0 => Trend::Increasing,
            x if x < -10.0 => Trend::Decreasing,
            _ => Trend::Stable,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TrendAnalysis {
    pub throughput_trend: Trend,
    pub latency_trend: Trend,
    pub error_rate_trend: Trend,
    pub memory_trend: Trend,
}

#[derive(Debug, Clone)]
pub enum Trend {
    Increasing,
    Decreasing,
    Stable,
}

impl Default for TrendAnalysis {
    fn default() -> Self {
        Self {
            throughput_trend: Trend::Stable,
            latency_trend: Trend::Stable,
            error_rate_trend: Trend::Stable,
            memory_trend: Trend::Stable,
        }
    }
}
```

## Best Practices

1. **Comprehensive metrics** - Monitor all key system components
2. **Structured logging** - Use consistent, searchable log formats
3. **Distributed tracing** - Track requests across service boundaries
4. **Proactive alerting** - Alert on trends, not just thresholds
5. **Performance baselines** - Establish and monitor performance baselines
6. **Dashboard organization** - Create role-specific dashboards
7. **Alert fatigue** - Tune alerts to reduce noise
8. **Runbook automation** - Automate common response procedures

## Summary

Monitoring an EventCore application:

- ✅ **`tracing` spans** - `execute()` is instrumented out of the box; subscribe
  with any `tracing-subscriber` layer
- ✅ **`MetricsHook` trait** - the library's hook for retry-lifecycle metrics,
  attached via `RetryPolicy::with_metrics_hook(...)`
- ✅ **Application-owned Prometheus metrics** - you define and export the
  `eventcore_*` series; the library does not
- ✅ **Structured logging** - wrap `execute()` to emit searchable, contextual logs
- ✅ **Distributed tracing** - propagate context and export EventCore's spans
- ✅ **Alerting & dashboards** - built on the metrics your service exposes

What EventCore gives you vs. what you own:

1. **EventCore provides:** `tracing` spans on `execute()`, the `MetricsHook`
   trait, and `ExecutionResponse::attempts()` for retry counts.
2. **You own:** the metrics registry/exporter, log subscriber configuration,
   OpenTelemetry wiring, alert rules, and dashboards.
3. There is no `eventcore::monitoring` module, no built-in metrics registry, no
   CLI, and no environment variables read by the library.
4. The write path is always `handle() -> NewEvents` committed by `execute()` —
   there is no separate `write_events`/`EventToWrite`/`WriteResult` API to log.

Next, let's explore [Backup and Recovery](./03-backup-recovery.md) →
