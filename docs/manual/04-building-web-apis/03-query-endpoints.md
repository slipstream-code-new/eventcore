# Chapter 4.3: Query Endpoints

Query endpoints serve read requests from your projections. Unlike commands that modify state, queries are side-effect free and can be cached, making them perfect for high-performance read operations.

## Query Architecture

```
HTTP Request → Authenticate → Authorize → Query Read Model → Format Response
                                                ↑
                                  Read Model (kept current by a Projector)
```

## Where EventCore Ends and Your Application Begins

EventCore owns the _write_ side and the machinery that keeps read models
current. It does **not** ship a query/registry layer for your HTTP handlers.
Two things matter for this chapter:

1. **EventCore's read API is a `Projector` driven by `run_projection`.** Your
   projector consumes events in order and writes whatever read model your
   queries need (a SQL table, a search index, an in-memory map, etc.). EventCore
   re-exports everything you need for this from the crate root:
   `eventcore::{Projector, run_projection, ProjectionConfig, StreamPosition,
FailureStrategy, FailureContext}`.

2. **The read store and the query methods on it are _your_ code.** The
   `AppState`, projection registries, `SearchQuery` builders, and the
   `get_all_tasks()` / `search()` / `calculate_statistics()` methods used by the
   handlers below are **application-level** examples — they are how _your_
   service exposes the read model your projector built. They are not EventCore
   APIs, and EventCore imposes no particular shape on them.

### The real EventCore read path

A projector turns the event log into whatever read model your queries serve.
EventCore calls `apply` for each event in stream-position order and persists a
checkpoint so it can resume:

```rust
use eventcore::{
    FailureContext, FailureStrategy, Projector, ProjectionConfig,
    StreamPosition, run_projection,
};

// Application-level read model the HTTP handlers will query.
#[derive(Default)]
struct TaskListReadModel {
    // ... your storage: a HashMap, a SQL pool, a search index, etc.
}

struct TaskListProjector {
    name: String,
    // a handle to where the read model is stored
}

impl Projector for TaskListProjector {
    type Event = TaskEvent;        // your domain event enum
    type Error = TaskProjectionError;
    type Context = TaskListReadModel;

    fn apply(
        &mut self,
        event: Self::Event,
        _position: StreamPosition,
        ctx: &mut Self::Context,
    ) -> Result<(), Self::Error> {
        // Update your read model from the event. New domain facts become
        // new enum variants over time; match them all so old projectors keep
        // compiling and unknown-but-additive fields use serde defaults.
        match event {
            TaskEvent::Created { .. } => { /* insert into read model */ }
            TaskEvent::StatusChanged { .. } => { /* update read model */ }
            // ...
        }
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }

    // Optional: decide what happens when apply() fails for one event.
    fn on_error(&mut self, _ctx: FailureContext<'_, Self::Error>) -> FailureStrategy {
        FailureStrategy::Fatal // the default; Skip and Retry are also available
    }
}

// Drive the projector. `backend` is any store that implements the reader and
// checkpoint contracts (the memory, sqlite, postgres, or fs backends).
// Default config is batch mode (process everything available, then stop);
// `.continuous()` keeps polling for new events.
async fn keep_read_model_current<B>(backend: &B) -> Result<(), eventcore::ProjectionError>
where
    B: eventcore_types::EventReader
        + eventcore_types::CheckpointStore
        + eventcore_types::ProjectorCoordinator,
{
    let projector = TaskListProjector { name: "task-list".to_string() };
    let config = ProjectionConfig::default().continuous();
    run_projection(projector, backend, config).await
}
```

Everything from here down is the _query_ side: HTTP handlers reading the model
your projector maintains. The exact storage and query methods are yours to
design — the examples use plausible application-level types so the patterns are
concrete.

## Basic Query Pattern

### Simple Query Endpoint

> **Application-level code.** The handler below reads from a read model your
> projector maintains (see the section above). `AppState`, the `read_models`
> accessor, and `get_all_tasks()` are example application types — not EventCore
> APIs. EventCore's only involvement here is having kept the read model current
> via `run_projection`.

```rust
use axum::{
    extract::{State, Path, Query as QueryParams},
    Json,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
struct ListTasksQuery {
    #[serde(default)]
    status: Option<TaskStatus>,

    #[serde(default)]
    assigned_to: Option<String>,

    #[serde(default = "default_page")]
    page: u32,

    #[serde(default = "default_page_size")]
    page_size: u32,
}

fn default_page() -> u32 { 1 }
fn default_page_size() -> u32 { 20 }

#[derive(Debug, Serialize)]
struct ListTasksResponse {
    tasks: Vec<TaskSummary>,
    pagination: PaginationInfo,
}

#[derive(Debug, Serialize)]
struct TaskSummary {
    id: String,
    title: String,
    status: TaskStatus,
    assigned_to: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct PaginationInfo {
    page: u32,
    page_size: u32,
    total_items: u64,
    total_pages: u32,
}

async fn list_tasks(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<ListTasksQuery>,
) -> Result<Json<ListTasksResponse>, ApiError> {
    // Read from the application's read model (maintained by a Projector via
    // run_projection). `read_models` and `task_list()` are app-level accessors.
    let read_model = state.read_models.task_list();

    // Apply filters
    let mut tasks = read_model.get_all_tasks();

    if let Some(status) = query.status {
        tasks.retain(|t| t.status == status);
    }

    if let Some(assigned_to) = query.assigned_to {
        tasks.retain(|t| t.assigned_to.as_ref() == Some(&assigned_to));
    }

    // Calculate pagination
    let total_items = tasks.len() as u64;
    let total_pages = ((total_items as f32) / (query.page_size as f32)).ceil() as u32;

    // Apply pagination
    let start = ((query.page - 1) * query.page_size) as usize;
    let end = (start + query.page_size as usize).min(tasks.len());
    let page_tasks = tasks[start..end].to_vec();

    Ok(Json(ListTasksResponse {
        tasks: page_tasks.into_iter().map(Into::into).collect(),
        pagination: PaginationInfo {
            page: query.page,
            page_size: query.page_size,
            total_items,
            total_pages,
        },
    }))
}
```

## Advanced Query Patterns

### Filtering and Sorting

> **Application-level code.** `SearchQuery`, the builder methods, and
> `read_model.search()` are illustrative application types showing how a
> read model _you_ designed might expose rich filtering. EventCore does not
> provide a query DSL — it provides the `Projector`/`run_projection` machinery
> that keeps the model your queries read from up to date.

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortField {
    CreatedAt,
    UpdatedAt,
    Title,
    Priority,
    DueDate,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Deserialize)]
struct AdvancedTaskQuery {
    // Filters
    #[serde(default)]
    status: Option<Vec<TaskStatus>>,

    #[serde(default)]
    assigned_to: Option<Vec<String>>,

    #[serde(default)]
    created_after: Option<DateTime<Utc>>,

    #[serde(default)]
    created_before: Option<DateTime<Utc>>,

    #[serde(default)]
    search: Option<String>,

    // Sorting
    #[serde(default = "default_sort_field")]
    sort_by: SortField,

    #[serde(default = "default_sort_order")]
    sort_order: SortOrder,

    // Pagination
    #[serde(default)]
    cursor: Option<String>,

    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_sort_field() -> SortField { SortField::CreatedAt }
fn default_sort_order() -> SortOrder { SortOrder::Desc }
fn default_limit() -> u32 { 50 }

async fn search_tasks(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<AdvancedTaskQuery>,
) -> Result<Json<CursorPaginatedResponse<TaskSummary>>, ApiError> {
    // App-level accessor for the search read model your projector populates.
    let read_model = state.read_models.task_search();

    // Build query
    let mut search_query = SearchQuery::new();

    if let Some(statuses) = query.status {
        search_query = search_query.with_status_in(statuses);
    }

    if let Some(assignees) = query.assigned_to {
        search_query = search_query.with_assignee_in(assignees);
    }

    if let Some(after) = query.created_after {
        search_query = search_query.created_after(after);
    }

    if let Some(before) = query.created_before {
        search_query = search_query.created_before(before);
    }

    if let Some(search_text) = query.search {
        search_query = search_query.with_text_search(search_text);
    }

    // Apply sorting
    search_query = match query.sort_by {
        SortField::CreatedAt => search_query.sort_by_created_at(query.sort_order),
        SortField::UpdatedAt => search_query.sort_by_updated_at(query.sort_order),
        SortField::Title => search_query.sort_by_title(query.sort_order),
        SortField::Priority => search_query.sort_by_priority(query.sort_order),
        SortField::DueDate => search_query.sort_by_due_date(query.sort_order),
    };

    // Apply cursor pagination
    if let Some(cursor) = query.cursor {
        search_query = search_query.after_cursor(Cursor::decode(&cursor)?);
    }

    search_query = search_query.limit(query.limit);

    // Execute query against the read model
    let results = read_model.search(search_query).await?;

    Ok(Json(results))
}
```

### Aggregation Queries

> **Application-level code.** Aggregations live in read models you design and
> keep current with a `Projector`. A common pattern is a dedicated analytics
> projector whose `apply` increments counters/rollups as events arrive, so the
> HTTP handler just reads a precomputed result.

```rust
#[derive(Debug, Serialize)]
struct TaskStatistics {
    total_tasks: u64,
    tasks_by_status: HashMap<TaskStatus, u64>,
    tasks_by_assignee: Vec<AssigneeStats>,
    completion_rate: f64,
    average_completion_time: Option<Duration>,
    overdue_tasks: u64,
}

#[derive(Debug, Serialize)]
struct AssigneeStats {
    assignee_id: String,
    assignee_name: String,
    total_tasks: u64,
    completed_tasks: u64,
    in_progress_tasks: u64,
}

async fn get_task_statistics(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<DateRangeQuery>,
) -> Result<Json<TaskStatistics>, ApiError> {
    // App-level analytics read model populated by an analytics projector.
    let read_model = state.read_models.task_analytics();

    let stats = read_model.calculate_statistics(
        query.start_date,
        query.end_date,
    ).await?;

    Ok(Json(stats))
}

// Time-series data
#[derive(Debug, Serialize)]
struct TimeSeriesData {
    period: String,
    data_points: Vec<DataPoint>,
}

#[derive(Debug, Serialize)]
struct DataPoint {
    timestamp: DateTime<Utc>,
    value: f64,
    metadata: Option<serde_json::Value>,
}

async fn get_task_completion_trend(
    State(state): State<AppState>,
    QueryParams(query): QueryParams<TimeSeriesQuery>,
) -> Result<Json<TimeSeriesData>, ApiError> {
    // App-level time-series read model populated by a metrics projector.
    let read_model = state.read_models.task_metrics();

    let data = read_model.get_completion_trend(
        query.start_date,
        query.end_date,
        query.granularity,
    ).await?;

    Ok(Json(data))
}
```

## GraphQL Integration

For complex queries, GraphQL can be more efficient. As before, the `TaskProjection`
/ `UserProjection` types injected into the GraphQL context are **application-level
read models** — your code, kept current by a `Projector`. EventCore is not part
of the GraphQL layer; it only maintained the read models these resolvers query.

```rust
use async_graphql::{
    Context, Object, Schema, EmptyMutation, EmptySubscription,
    ID, Result as GraphQLResult,
};

struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn task(&self, ctx: &Context<'_>, id: ID) -> GraphQLResult<Option<Task>> {
        let projection = ctx.data::<Arc<TaskProjection>>()?;

        Ok(projection.get_task(&id.to_string()).await?)
    }

    async fn tasks(
        &self,
        ctx: &Context<'_>,
        filter: Option<TaskFilter>,
        sort: Option<TaskSort>,
        pagination: Option<PaginationInput>,
    ) -> GraphQLResult<TaskConnection> {
        let projection = ctx.data::<Arc<TaskProjection>>()?;

        let query = build_query(filter, sort, pagination);
        let results = projection.query(query).await?;

        Ok(TaskConnection::from(results))
    }

    async fn user(&self, ctx: &Context<'_>, id: ID) -> GraphQLResult<Option<User>> {
        let projection = ctx.data::<Arc<UserProjection>>()?;

        Ok(projection.get_user(&id.to_string()).await?)
    }
}

// GraphQL types
#[derive(async_graphql::SimpleObject)]
struct Task {
    id: ID,
    title: String,
    description: String,
    status: TaskStatus,
    assigned_to: Option<User>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(async_graphql::InputObject)]
struct TaskFilter {
    status: Option<Vec<TaskStatus>>,
    assigned_to: Option<Vec<ID>>,
    created_after: Option<DateTime<Utc>>,
    search: Option<String>,
}

// Axum handler
async fn graphql_handler(
    State(state): State<AppState>,
    user: Option<AuthenticatedUser>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(state.read_models.clone())
        .data(user)
        .finish();

    schema.execute(req.into_inner()).await.into()
}
```

## Caching Strategies

### Response Caching

```rust
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use sha2::{Sha256, Digest};

#[derive(Clone)]
struct CacheConfig {
    public_max_age: Duration,
    private_max_age: Duration,
    stale_while_revalidate: Duration,
}

async fn cached_query_handler<F, Fut, T>(
    headers: HeaderMap,
    cache_config: CacheConfig,
    query_fn: F,
) -> Response
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
    T: Serialize,
{
    // Execute query
    let result = match query_fn().await {
        Ok(data) => data,
        Err(e) => return e.into_response(),
    };

    // Serialize response
    let body = match serde_json::to_vec(&result) {
        Ok(bytes) => bytes,
        Err(_) => return ApiError::internal("Serialization failed").into_response(),
    };

    // Calculate ETag
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let etag = format!("\"{}\"", hex::encode(hasher.finalize()));

    // Check If-None-Match
    if let Some(if_none_match) = headers.get(IF_NONE_MATCH) {
        if if_none_match.to_str().ok() == Some(&etag) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }

    // Build response with caching headers
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .header(ETAG, &etag)
        .header(
            CACHE_CONTROL,
            format!(
                "public, max-age={}, stale-while-revalidate={}",
                cache_config.public_max_age.as_secs(),
                cache_config.stale_while_revalidate.as_secs()
            )
        )
        .body(Body::from(body))
        .unwrap()
}

// Usage
async fn get_public_statistics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    cached_query_handler(
        headers,
        CacheConfig {
            public_max_age: Duration::from_secs(300), // 5 minutes
            private_max_age: Duration::from_secs(0),
            stale_while_revalidate: Duration::from_secs(60),
        },
        || async {
            // App-level read model populated by a public-stats projector.
            let read_model = state.read_models.public_stats();
            read_model.get_current_stats().await
        },
    ).await
}
```

### Query Result Caching

```rust
use moka::future::Cache;

#[derive(Clone)]
struct QueryCache {
    cache: Cache<String, CachedResult>,
}

#[derive(Clone)]
struct CachedResult {
    data: Vec<u8>,
    cached_at: DateTime<Utc>,
    ttl: Duration,
}

impl QueryCache {
    fn new() -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(300))
                .build(),
        }
    }

    async fn get_or_compute<F, Fut, T>(
        &self,
        key: &str,
        ttl: Duration,
        compute_fn: F,
    ) -> Result<T, ApiError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
        T: Serialize + DeserializeOwned,
    {
        // Check cache
        if let Some(cached) = self.cache.get(key).await {
            if Utc::now() - cached.cached_at < cached.ttl {
                return serde_json::from_slice(&cached.data)
                    .map_err(|_| ApiError::internal("Cache deserialization failed"));
            }
        }

        // Compute result
        let result = compute_fn().await?;

        // Cache result
        let data = serde_json::to_vec(&result)
            .map_err(|_| ApiError::internal("Cache serialization failed"))?;

        self.cache.insert(
            key.to_string(),
            CachedResult {
                data,
                cached_at: Utc::now(),
                ttl,
            }
        ).await;

        Ok(result)
    }
}
```

## Real-time Queries with SSE

Server-Sent Events for live updates. EventCore does not provide a push/subscribe
API — the closest mechanism is a **continuous** projection
(`ProjectionConfig::default().continuous()`), which keeps polling for new events
and applying them to your read model. To stream updates to clients, have your
projector (or the read model it writes to) publish change notifications on a
channel your SSE handler subscribes to. The `subscribe_to_updates` method below
is **application-level** — it is your read model's notification API, not an
EventCore API.

```rust
use axum::response::sse::{Event, Sse};
use futures::stream::Stream;
use tokio_stream::StreamExt;

async fn task_updates_stream(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Sse<impl Stream<Item = Result<Event, ApiError>>> {
    let stream = async_stream::stream! {
        // App-level: subscribe to change notifications published by the read
        // model (which a continuous projector keeps current). EventCore has no
        // built-in subscription API; this is your service's notification layer.
        let mut subscription = state.read_models
            .task_list()
            .subscribe_to_updates(user.id)
            .await;

        while let Some(update) = subscription.next().await {
            let event = match update {
                TaskUpdate::Created(task) => {
                    Event::default()
                        .event("task-created")
                        .json_data(task)
                        .unwrap()
                }
                TaskUpdate::Updated(task) => {
                    Event::default()
                        .event("task-updated")
                        .json_data(task)
                        .unwrap()
                }
                TaskUpdate::Deleted(task_id) => {
                    Event::default()
                        .event("task-deleted")
                        .data(task_id)
                }
            };

            yield Ok(event);
        }
    };

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(30))
            .text("keep-alive")
    )
}
```

## Query Performance Optimization

### N+1 Query Prevention

The `TaskProjection` here is the same application-level read model used
throughout this chapter; the batching advice applies to whatever storage backs
it.

```rust
// Bad: N+1 queries
async fn get_tasks_with_assignees_bad(
    projection: &TaskProjection,
) -> Result<Vec<TaskWithAssignee>, ApiError> {
    let tasks = projection.get_all_tasks().await?;
    let mut results = Vec::new();

    for task in tasks {
        // This makes a separate query for each task!
        let assignee = if let Some(assignee_id) = &task.assigned_to {
            projection.get_user(assignee_id).await?
        } else {
            None
        };

        results.push(TaskWithAssignee {
            task,
            assignee,
        });
    }

    Ok(results)
}

// Good: Batch loading
async fn get_tasks_with_assignees_good(
    projection: &TaskProjection,
) -> Result<Vec<TaskWithAssignee>, ApiError> {
    let tasks = projection.get_all_tasks().await?;

    // Collect all assignee IDs
    let assignee_ids: HashSet<_> = tasks
        .iter()
        .filter_map(|t| t.assigned_to.as_ref())
        .cloned()
        .collect();

    // Load all assignees in one query
    let assignees = projection
        .get_users_by_ids(assignee_ids.into_iter().collect())
        .await?;

    // Build results
    let assignee_map: HashMap<_, _> = assignees
        .into_iter()
        .map(|u| (u.id.clone(), u))
        .collect();

    Ok(tasks.into_iter().map(|task| {
        let assignee = task.assigned_to
            .as_ref()
            .and_then(|id| assignee_map.get(id))
            .cloned();

        TaskWithAssignee { task, assignee }
    }).collect())
}
```

### Query Complexity Limits

```rust
use async_graphql::{extensions::ComplexityLimit, ValidationResult};

struct QueryComplexity;

impl QueryComplexity {
    fn calculate_complexity(query: &GraphQLQuery) -> u32 {
        // Simple heuristic: count fields and multiply by depth
        let field_count = count_fields(query);
        let max_depth = calculate_max_depth(query);

        field_count * max_depth
    }
}

// In GraphQL schema
let schema = Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
    .extension(ComplexityLimit::new(1000)) // Max complexity
    .finish();

// For REST endpoints
#[derive(Debug)]
struct QueryComplexityGuard {
    max_items: u32,
    max_depth: u32,
}

impl QueryComplexityGuard {
    fn validate(&self, query: &AdvancedTaskQuery) -> Result<(), ApiError> {
        // Check pagination limits
        if query.limit > self.max_items {
            return Err(ApiError::bad_request(
                format!("Limit cannot exceed {}", self.max_items)
            ));
        }

        // Check filter complexity
        let filter_count =
            query.status.as_ref().map(|s| s.len()).unwrap_or(0) +
            query.assigned_to.as_ref().map(|a| a.len()).unwrap_or(0);

        if filter_count > 100 {
            return Err(ApiError::bad_request(
                "Too many filter values"
            ));
        }

        Ok(())
    }
}
```

## Security Considerations

### Query Authorization

```rust
trait QueryAuthorizer {
    async fn can_view_task(&self, user: &AuthenticatedUser, task_id: &str) -> bool;
    async fn can_view_user_tasks(&self, user: &AuthenticatedUser, target_user_id: &str) -> bool;
    async fn can_view_statistics(&self, user: &AuthenticatedUser) -> bool;
}

struct RoleBasedAuthorizer;

impl QueryAuthorizer for RoleBasedAuthorizer {
    async fn can_view_task(&self, user: &AuthenticatedUser, task_id: &str) -> bool {
        // Admin can see all
        if user.has_role("admin") {
            return true;
        }

        // Others can only see their own tasks or tasks they created
        // Would need to check task details...
        true
    }

    async fn can_view_user_tasks(&self, user: &AuthenticatedUser, target_user_id: &str) -> bool {
        // Users can see their own tasks
        if user.id.to_string() == target_user_id {
            return true;
        }

        // Managers can see their team's tasks
        user.has_role("manager") || user.has_role("admin")
    }

    async fn can_view_statistics(&self, user: &AuthenticatedUser) -> bool {
        user.has_role("manager") || user.has_role("admin")
    }
}

// Use in handlers
async fn get_user_tasks(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<TaskSummary>>, ApiError> {
    // Check authorization
    if !state.authorizer.can_view_user_tasks(&user, &user_id).await {
        return Err(ApiError::forbidden("Cannot view tasks for this user"));
    }

    // Continue with query...
}
```

### Rate Limiting

```rust
use governor::{Quota, RateLimiter};

#[derive(Clone)]
struct RateLimitConfig {
    anonymous_quota: Quota,
    authenticated_quota: Quota,
    admin_quota: Quota,
}

async fn rate_limit_middleware(
    State(limiter): State<Arc<RateLimiter<String>>>,
    user: Option<AuthenticatedUser>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let key = match &user {
        Some(u) => u.id.to_string(),
        None => request
            .headers()
            .get("x-forwarded-for")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("anonymous")
            .to_string(),
    };

    limiter
        .check_key(&key)
        .map_err(|_| ApiError::too_many_requests("Rate limit exceeded"))?;

    Ok(next.run(request).await)
}
```

## Testing Query Endpoints

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pagination() {
        let state = create_test_state_with_tasks(100).await;

        // First page
        let response = list_tasks(
            State(state.clone()),
            QueryParams(ListTasksQuery {
                page: 1,
                page_size: 20,
                ..Default::default()
            }),
        ).await.unwrap();

        assert_eq!(response.0.tasks.len(), 20);
        assert_eq!(response.0.pagination.total_items, 100);
        assert_eq!(response.0.pagination.total_pages, 5);

        // Last page
        let response = list_tasks(
            State(state),
            QueryParams(ListTasksQuery {
                page: 5,
                page_size: 20,
                ..Default::default()
            }),
        ).await.unwrap();

        assert_eq!(response.0.tasks.len(), 20);
    }

    #[tokio::test]
    async fn test_caching_headers() {
        let state = create_test_state().await;

        let response = get_public_statistics(
            State(state),
            HeaderMap::new(),
        ).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(ETAG));
        assert!(response.headers().contains_key(CACHE_CONTROL));

        let cache_control = response.headers()
            .get(CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap();

        assert!(cache_control.contains("max-age=300"));
    }
}
```

## Best Practices

1. **Read from purpose-built read models** - Drive them with a `Projector` +
   `run_projection`. Direct event reads exist (`EventStore::read_stream` plus the
   `collect_events` helper), but they replay raw history and are best reserved
   for tests, debugging, or building a projection — not for serving queries.
2. **Paginate results** - Never return unbounded lists
3. **Cache aggressively** - Read queries are perfect for caching
4. **Validate query parameters** - Prevent resource exhaustion
5. **Monitor performance** - Track slow queries
6. **Use appropriate protocols** - REST for simple, GraphQL for complex
7. **Implement authorization** - Check permissions for all queries
8. **Version your API** - Queries can evolve independently

## Summary

Query endpoints in EventCore applications:

- ✅ **Read-model-based** - Serve queries from read models a `Projector` keeps
  current via `run_projection`, not from raw event streams
- ✅ **Performant** - Caching and optimization live in your application layer
- ✅ **Flexible** - Support REST, GraphQL, and real-time
- ✅ **Secure** - Authorization and rate limiting (your application's concern)
- ✅ **Testable** - Easy to test in isolation

Remember the boundary: EventCore supplies the `Projector` / `run_projection` /
`ProjectionConfig` machinery and the underlying event store. The read-model
storage, query methods, registries, caching, and HTTP handlers shown here are
application-level code — EventCore imposes no shape on them.

Key patterns:

1. Read from read models (built by projectors), not raw event streams
2. Implement proper pagination
3. Cache responses appropriately
4. Validate and limit query complexity
5. Authorize access to data
6. Monitor query performance

Next, let's explore [Authentication and Authorization](./04-authentication.md) →
