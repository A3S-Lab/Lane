# a3s-lane

Lane-based priority queues for concurrent async tasks. Commands can be organized into named lanes with configurable concurrency and priority, or retained as typed host-owned values until the host is ready to execute them.

Priority controls which pending item is admitted next. It does not interrupt an already-running future; active work still needs an explicit cancellation and settlement contract.

[![crates.io](https://img.shields.io/crates/v/a3s-lane.svg)](https://crates.io/crates/a3s-lane)

## Install

```toml
[dependencies]
a3s-lane = "0.5"
```

All four features (`distributed`, `metrics`, `monitoring`, `telemetry`) are on by default. Core queue only:

```toml
a3s-lane = { version = "0.5", default-features = false }
# or pick selectively:
a3s-lane = { version = "0.5", default-features = false, features = ["metrics", "distributed"] }
```

Enable the optional Redis generic job backend for multi-process workers:

```toml
a3s-lane = { version = "0.5", features = ["redis-backend"] }
```

## Usage

Implement the `Command` trait for each task type:

```rust
#[async_trait]
pub trait Command: Send + Sync {
    async fn execute(&self) -> Result<serde_json::Value>;
    fn command_type(&self) -> &str;
}
```

Then build a manager, start the scheduler, and submit:

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, Command, Result};
use async_trait::async_trait;
use std::time::Duration;

struct FetchCommand { url: String }

#[async_trait]
impl Command for FetchCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "url": self.url }))
    }
    fn command_type(&self) -> &str { "fetch" }
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build().await?;

    manager.start().await?;

    let rx = manager.submit("query", Box::new(FetchCommand { url: "...".into() })).await?;
    let result = rx.await??;
    println!("{result}");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;
    Ok(())
}
```

`submit()` returns a `oneshot::Receiver<Result<Value>>` — the `??` unwraps both the channel send and the command result.

## Lane model

| Lane | Priority | Max concurrency | Use case |
|------|----------|-----------------|----------|
| `system` | 0 (highest) | 5 | System-level ops |
| `control` | 1 | 3 | Pause / cancel |
| `query` | 2 | 10 | Read-only queries |
| `session` | 3 | 5 | Session management |
| `skill` | 4 | 3 | Tool execution |
| `prompt` | 5 (lowest) | 2 | LLM generation |

Custom lanes replace or extend the defaults:

```rust
QueueManagerBuilder::new(emitter)
    .with_lane("high",  LaneConfig::new(1, 4), 0)
    .with_lane("low",   LaneConfig::new(1, 2), 1)
    .build().await?;
```

## Host-owned typed queue

Use `PriorityQueue<T>` when the host must keep ownership of typed state and
decide when execution starts, as a terminal or web event loop does. Lower
numeric values run first and equal-priority items remain FIFO:

```rust
use a3s_lane::{PriorityItem, PriorityQueue};

let mut turns = PriorityQueue::new();
turns.push(1, "automatic continuation");
turns.push(0, "first user turn");
turns.push(0, "second user turn");

let claimed = turns.pop().expect("queued turn");
assert_eq!(claimed.value(), &"first user turn");

// If admission fails before execution starts, preserve its original FIFO slot.
turns.restore(claimed);
let order = turns
    .ordered()
    .into_iter()
    .map(PriorityItem::value)
    .copied()
    .collect::<Vec<_>>();
assert_eq!(
    order,
    ["first user turn", "second user turn", "automatic continuation"]
);
```

`ordered()` is a non-mutating projection for queue UIs. A claimed item keeps
its priority and insertion sequence, so `restore()` can put a failed admission
back without moving it behind newer work.

## LaneConfig

All options use the builder pattern and can be chained:

```rust
LaneConfig::new(min_concurrency, max_concurrency)
    .with_timeout(Duration::from_secs(30))
    .with_retry_policy(RetryPolicy::exponential(3))     // 100ms initial, 2× backoff, 30s cap
    .with_pressure_threshold(50)                        // emit queue.lane.pressure / queue.lane.idle
    .with_rate_limit(RateLimitConfig::per_second(100))  // requires `distributed` feature
    .with_priority_boost(PriorityBoostConfig::standard( // requires `distributed` feature
        Duration::from_secs(300),
    ))
```

**RetryPolicy**: `exponential(max_retries)`, `fixed(max_retries, delay)`, `none()`.

**RateLimitConfig**: `per_second(n)`, `per_minute(n)`, `per_hour(n)`, `unlimited()`.

**PriorityBoostConfig**: `standard(deadline)` (boosts at 75/50/25% of deadline remaining), `aggressive(deadline)`, `disabled()`.

## Events

`EventStream` implements `futures_core::Stream` — use `.next().await` via `StreamExt` or the `.recv()` convenience method. Subscribe directly from the manager without threading `EventEmitter` manually:

```rust
use tokio_stream::StreamExt;

// All events
let mut stream = manager.subscribe();

// Filtered — only failures
let mut failures = manager.subscribe_filtered(|e| {
    e.key == "queue.command.failed" || e.key == "queue.command.timeout"
});

tokio::spawn(async move {
    while let Some(event) = stream.next().await {
        println!("[{}] {}", event.timestamp, event.key);
    }
});
```

Events emitted automatically at every queue stage:

| Event key | When | Payload fields |
|-----------|------|----------------|
| `queue.command.submitted` | `submit()` accepted | `lane_id` |
| `queue.command.started` | Scheduler dispatched | `lane_id`, `command_id`, `command_type` |
| `queue.command.completed` | Returned `Ok` | `lane_id`, `command_id` |
| `queue.command.retry` | Failed, will retry | `lane_id`, `command_id`, `attempt` |
| `queue.command.dead_lettered` | Moved to DLQ | `lane_id`, `command_id`, `command_type` |
| `queue.command.failed` | Terminal failure | `lane_id`, `command_id`, `error` |
| `queue.command.timeout` | Timed out | `lane_id`, `command_id`, `error` |
| `queue.shutdown.started` | `shutdown()` called | — |
| `queue.lane.pressure` | `pending >= threshold`, first crossing | `lane_id` |
| `queue.lane.idle` | `pending == 0` after being pressured | `lane_id` |

`queue.lane.pressure` and `queue.lane.idle` require `with_pressure_threshold(n)` on the lane config.

## Reliability

### Dead letter queue

```rust
let dlq = DeadLetterQueue::new(1000);
let queue = CommandQueue::with_dlq(emitter, dlq.clone());

// Inspect failed commands after running
for letter in dlq.list().await {
    println!("{}: {}", letter.command_type, letter.error);
}
```

### Persistent storage

```rust
let storage = Arc::new(LocalStorage::new(PathBuf::from("./queue_data")).await?);
let manager = QueueManagerBuilder::new(emitter)
    .with_storage(storage)
    .with_default_lanes()
    .build().await?;
```

Custom backends: implement the `Storage` trait (`save_command`, `load_commands`, `remove_command`, `save_dead_letter`, `load_dead_letters`, `clear_all`).

### Graceful shutdown

```rust
manager.shutdown().await;                           // stop accepting new commands
manager.drain(Duration::from_secs(30)).await?;      // wait for in-flight to finish
```

## Observability

### Metrics

```rust
let metrics = QueueMetrics::local();  // in-memory; or bring your own MetricsBackend
let manager = QueueManagerBuilder::new(emitter)
    .with_metrics(metrics.clone())
    .build().await?;

let snap = metrics.snapshot().await;
// snap.counters  →  submit/complete/fail/timeout/retry/dead-letter counts per lane
// snap.histograms →  latency p50/p90/p95/p99 per lane
```

OpenTelemetry OTLP export: use `OtelMetricsBackend` (requires `telemetry` feature).

Custom backend: implement `MetricsBackend` (`increment_counter`, `set_gauge`, `record_histogram`, `snapshot`, `reset`).

### Alerts and monitoring

```rust
let alerts = Arc::new(AlertManager::with_queue_depth_alerts(
    100,  // warning threshold
    200,  // critical threshold
));
alerts.add_callback(|a| eprintln!("[{:?}] {}: {}", a.level, a.lane_id, a.message)).await;

let manager = QueueManagerBuilder::new(emitter)
    .with_alerts(alerts)
    .build().await?;
```

Background monitor (polls on an interval):

```rust
let monitor = Arc::new(QueueMonitor::with_config(manager.queue(), MonitorConfig {
    interval: Duration::from_secs(5),
    pending_warning_threshold: 50,
    active_warning_threshold: 25,
}));
monitor.clone().start().await;

let stats = monitor.stats().await;
println!("pending={} active={}", stats.total_pending, stats.total_active);
```

## Scalability (`distributed` feature)

```rust
// Rate limiting — enforced at dequeue time, not submit time
LaneConfig::new(1, 10).with_rate_limit(RateLimitConfig::per_second(100))

// Priority boost — commands approaching their deadline get elevated priority
LaneConfig::new(1, 10).with_priority_boost(
    PriorityBoostConfig::standard(Duration::from_secs(300))
)

// Multi-core partitioning — auto-detects CPU cores
let queue = Arc::new(LocalDistributedQueue::auto());
```

Custom distributed queue: implement `DistributedQueue` (`enqueue`, `dequeue`, `complete`, `num_partitions`, `worker_id`).

## Development

```bash
just test       # 420 library tests, --all-features
just ci         # fmt + clippy + test
just bench      # Criterion benchmarks → target/criterion/report/index.html
just cov        # coverage report (requires cargo-llvm-cov)
just doc        # generate and open rustdoc
```

Optional: `cargo install cargo-llvm-cov`, `brew install lcov` (HTML coverage).

## In the A3S ecosystem

a3s-lane is the scheduling layer of the A3S Agent OS. A3S Code uses the typed
queue for host-owned pending turns and can optionally create a per-session lane
manager for tool execution. Conversation execution itself remains
single-flight: cancellation settles the active worker before the next queued
turn is admitted.

```
a3s-gateway → a3s-box (MicroVM) → SafeClaw → a3s-code → a3s-lane
                                                          ↑ here
```

Works standalone for any priority-based async scheduling: web servers, background job processors, rate-limited API clients.

## Universal job queue roadmap

A3S Lane is evolving from an in-process lane scheduler into a general
distributed priority job queue. The direction is BullMQ-like, but native to the
A3S stack and Rust API.

| Phase | Status | Scope |
| --- | --- | --- |
| Lane scheduler | Done | Lane priorities, per-lane concurrency, command retries, timeout, DLQ, events, metrics, monitoring. |
| Generic job runtime | In progress | JSON jobs, Lua-backed Redis bulk submission, idempotent custom job IDs, simple deduplication with optional TTL, debounce TTL extension, delayed-owner replace, and keep-last-if-active requeue, repeat-key ownership and upsert, explicit job states, priority plus FIFO/LIFO same-priority ordering, finished-job retention by age/count/limit, retained queue event streams, delayed jobs, token-owned worker leases, active-to-wait/delayed movement, completion/failure snapshots, retry backoff, Redis-shared rate-limit and active-concurrency controls, BullMQ-style two-phase stalled recovery with repeat scheduler requeue handling, pause/resume. |
| Job management API | In progress | Add/get/get-state/get-job-finished-result/get-job-counts/get-job-count/count-pending/remove/remove-repeat/upsert-repeat/remove-deduplication-key/get-deduplication-job-id/list-repeats/get-repeat/count-repeats/list-repeats-page/add-flow-children/get-flow-dependencies/get-flow-dependency-counts/get-flow-dependency-selected-counts/get-flow-dependency-values/get-flow-dependency-page/get-flow-dependency-pages/get-flow-children-values/get-flow-ignored-children-failures/remove-unprocessed-children/remove-child-dependency/promote/reschedule/delay-active/release-active/retry/update-priority/update-priority-with-lifo/update-data/save-stacktrace/pause/resume/is-paused/drain/clean/obliterate/remove-orphaned Redis maintenance APIs, multi-state pagination, ascending/descending listing, waiting priority counts, add-log/get-logs/clear-job-logs, read-events/trim-events, progress updates, single and bulk lease renewal, Redis terminal metrics. |
| Worker runtime | In progress | `JobWorker` claims jobs from any `JobQueueBackend`, uses backend-native blocking claim hooks when available, routes jobs by name with `JobProcessorRouter`, runs async processors, completes/fails jobs, supports processor progress/log updates, cooperative lease-loss checks, timeouts, shared batch lease renewal for background loops, and stalled recovery loops. |
| Durable backend | In progress | `LocalJobQueue` JSON snapshot persistence is available, including parent-scoped flow dependency side indexes; `RedisJobQueue` is available behind `redis-backend` with Lua-backed add, bulk add, FIFO/LIFO waiting score ordering, BullMQ-style Redis worker marker zset updates, Redis marker-backed blocking claim, Redis stream queue events, simple deduplication with TTL, debounce TTL extension, delayed-owner replace, keep-last-if-active requeue, deduplication-key removal, repeat-key ownership, Redis-backed repeat scheduler zset/hash metadata, listing/removal/upsert/pagination, static flow submission, dynamic flow child fan-out, flow dependency inspection, BullMQ-style selected/full dependency bucket counts and reads, single/multi-bucket paginated dependency reads, flow child-value and ignored-failure reads, dynamic flow child deduplication skip and keep-last materialization, flow parent and active flow-child keep-last materialization, delayed promotion and rescheduling, active-to-wait/delayed movement, single-job promote, state-index and finished-result queries, job count snapshots, terminal metrics, manual retry, priority update, progress update, stacktrace update, log append, list/stat snapshots, finished-job age/count retention during complete/fail/stalled scripts, drain, clean, orphaned-job cleanup, obliterate, claim, Redis-shared rate limit, max-active, flow parent release/failure events, repeat successor enqueue, complete, fail, renew, remove, and stalled candidate-set recovery semantics. Postgres/NATS backends remain planned. |
| Flow jobs | In progress | Parent-child dependencies, waiting-children state, dependency inspection, BullMQ-style selected/full dependency bucket counts and reads, single/multi-bucket paginated dependency inspection, child return-value inspection, ignored, removed, continued, and fail-parent child-failure release, static and dynamic fan-out, fan-in release, flow parent deduplication events, static and dynamic ordinary flow child deduplication skip semantics, active flow-child keep-last deduplication materialization, BullMQ-style existing parent and child custom job-id attachment with `duplicated` events, in-memory/local flow-parent keep-last deduplication, and Redis flow-parent keep-last materialization on active parent completion, terminal failure, or stalled terminal failure are available. |
| Repeat jobs | In progress | Fixed-interval and UTC cron repeatable jobs with repeat keys, limits, end timestamps, repeat-key removal, upsert, single-key lookup, counts, and BullMQ-style next-time pagination are available across in-memory, local durable, and Redis backends. Redis additionally maintains scheduler zset/hash metadata in Lua so distributed readers and writers share one repeat-series state machine. |
| Framework integrations | Planned | NestJS module and migration guide from BullMQ-compatible concepts. |

The generic job runtime is exposed through the `JobQueueBackend` trait.
`InMemoryJobQueue` is process-local and intended for tests, embedded runtimes,
and reference semantics:

```rust
use a3s_lane::{InMemoryJobQueue, JobListOptions, JobOptions, JobQueueBackend, JobState, RetryPolicy};
use std::time::Duration;

# async fn example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("email");

let job = queue
    .add(
        "send",
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new()
            .with_job_id("email:ops@example.com:welcome")
            .with_priority(10)
            .with_lifo(false)
            .with_delay(Duration::from_secs(5))
            .with_retry_policy(RetryPolicy::fixed(3, Duration::from_secs(1))),
    )
    .await?;

let bulk = queue
    .add_jobs(
        vec![
            a3s_lane::JobSpec::new("index", serde_json::json!({ "id": 1 })),
            a3s_lane::JobSpec::new("index", serde_json::json!({ "id": 2 })),
        ],
        chrono::Utc::now(),
    )
    .await?;
assert_eq!(bulk.len(), 2);

let recent_pending = queue
    .list_jobs(
        JobListOptions::new()
            .with_states([JobState::Waiting, JobState::Delayed])
            .descending()
            .with_limit(20),
    )
    .await?;
assert_eq!(recent_pending.total, 3);

queue.promote_due_jobs(chrono::Utc::now()).await?;
let claimed = queue
    .claim_next("worker-1".to_string(), Duration::from_secs(30), chrono::Utc::now())
    .await?;

if let Some(claimed) = claimed {
    let lock_token = claimed
        .lock_token
        .as_deref()
        .expect("claimed jobs include a lock token");
    queue
        .update_data(&claimed.id, serde_json::json!({ "to": "ops@example.com", "normalized": true }))
        .await?;
    queue
        .update_progress(&claimed.id, serde_json::json!({ "percent": 50 }))
        .await?;
    queue
        .add_log(&claimed.id, "smtp accepted message".to_string(), 100, chrono::Utc::now())
        .await?;
    queue
        .complete_job(
            &claimed.id,
            lock_token,
            serde_json::json!({ "ok": true }),
            chrono::Utc::now(),
        )
        .await?;
}
# Ok(())
# }
```

Management APIs are part of the backend contract: `list_jobs()` returns
paginated `JobListPage` values with single-state, multi-state, ascending, and
descending range options, `add_jobs()` submits a batch with the same
idempotency semantics as `add_job()`, `promote_job()` moves delayed jobs to
waiting, `reschedule_job()` changes a delayed job's due time relative to the
current clock, `delay_active_job()` moves a token-owned active job back to
delayed, `release_active_job()` moves a token-owned active job back to waiting,
`get_job_state()` returns the current lifecycle state for a job id, `retry_job()`
manually requeues retained failed or completed jobs, `fail_job_discarding_retry()`
fails an active token-owned job without applying remaining automatic retries, `update_priority()`
changes stored job priority, `update_priority_with_lifo()` also chooses the
same-priority waiting reinsert side, `renew_lease()` extends an active worker
lease with the claim token, `renew_leases()` renews multiple claimed
leases and returns the job ids that failed renewal,
`remove_job()` removes jobs that are not protected by an active worker lock,
`remove_repeat()` removes the current non-active owner for a repeat key and, in
Redis, can fall back to scheduler metadata when the owner key is stale or
missing,
`upsert_repeat()` creates or replaces the current non-active owner for a repeat
key,
`remove_deduplication_key()` clears the active owner for a deduplication id,
`get_deduplication_job_id()` returns the current owner job id for a
deduplication id, `list_repeats()` lists current non-terminal repeat-series
owners, `get_repeat()` returns one current repeat owner by key,
`count_repeats()` returns the current repeat-series count, and
`list_repeats_page()` returns repeat series ordered by next scheduled time with
BullMQ-style default descending pagination,
`get_flow_dependencies()` returns a flow parent's child snapshots plus pending
and missing child ids, `get_flow_dependency_counts()` returns processed,
unprocessed, failed, ignored, and missing child counts,
`get_flow_dependency_selected_counts()` returns only the requested BullMQ-style
processed, unprocessed, ignored, and failed count buckets,
`get_flow_dependency_values()` returns BullMQ-style processed, unprocessed,
ignored, and failed dependency buckets,
`get_flow_dependency_page()` returns one BullMQ-style cursor page from a
processed, unprocessed, ignored, or failed flow dependency bucket, and
`get_flow_dependency_pages()` returns several requested dependency bucket pages
in one backend call,
flow parents cannot be completed while blocking child dependencies remain,
`add_flow_children()` lets an active, token-owned parent add new or existing
custom-id child jobs and move itself to `waiting_children`, mirroring BullMQ's
dynamic `moveToWaitingChildren()` fan-out path,
`remove_unprocessed_children()`
removes children that are still unprocessed and not active,
`remove_child_dependency()` detaches one child dependency from its parent
without deleting the child job,
`drain_jobs(false)` removes waiting jobs, `drain_jobs(true)` also removes
ordinary delayed jobs while preserving current delayed repeat owners,
`clean_jobs()` removes old records by state, `obliterate(false)` pauses the
queue and removes all queue data only when no active jobs exist,
`obliterate(true)` forces removal even with active jobs, `get_job_counts()`
returns per-state counts, `get_job_count()` returns aggregate counts for
selected states, `count_pending_jobs()` returns waiting, delayed, and
waiting-children work, `get_counts_per_priority()` returns waiting-job counts
for selected priorities, `get_job_finished_result()` returns `NotFinished`,
completed return values, or terminal failure reasons for retained jobs,
`RedisJobQueue::get_metrics()` returns BullMQ-style completed/failed
per-minute terminal metrics, `update_data()` replaces a retained job payload,
`save_stacktrace()` stores retained failure stack traces and a failure reason, `add_log()` appends retained job logs, and
`get_job_logs()` returns a `JobLogPage` with Redis/BullMQ-style range semantics.
`clear_job_logs(job_id, 0)` clears retained logs for a job, while positive
values keep the newest entries. `read_events("-", "+", limit)` reads retained
queue events in Redis stream id order, and `trim_events(max_len)` trims the
queue event stream using the backend's retained-event mechanism.
`pause()`, `resume()`, and `is_paused()` provide queue-level dispatch control.
Cleanup paths can unblock flow parents when a pending child is removed.
Set `JobOptions::with_job_id()` when producers need idempotent submission:
adding the same job id again returns the existing job instead of enqueueing a
duplicate. Custom job ids must not be `0` or start with `0:` because BullMQ
reserves that shape for internal waiting-list markers, and pure integer custom
ids are rejected to match BullMQ's `Job.validateOptions()` guard.
`JobOptions::with_lifo(true)` changes the ready-job insertion semantics for
jobs with the same priority: newer ready jobs are claimed before older ready
jobs, while lower priority values still run first. Priorities follow BullMQ's
integer range and must not exceed `2^21`; both add-time options and
`update_priority()`/`update_priority_with_lifo()` enforce that limit before
mutating backend state.
Finished jobs are retained by default. `remove_on_complete(true)` and
`remove_on_fail(true)` remain compatibility shorthands for deleting the current
terminal job immediately, matching BullMQ's `removeOnComplete: true` and
`removeOnFail: true`. Use `JobRetention` for BullMQ-style `KeepJobs` retention:
with TTL-backed deduplication, Redis still keeps the raw deduplication owner key
until its TTL expires even when the finished job record is removed immediately.
`count` keeps the newest N completed or failed jobs, `age` evicts jobs older
than a duration when another job reaches the same terminal state, and `limit`
bounds each age-cleanup pass.

```rust
# use a3s_lane::{JobOptions, JobRetention};
# use std::time::Duration;
let options = JobOptions::new()
    .with_completion_retention(JobRetention::count(1_000))
    .with_failure_retention(
        JobRetention::age_and_count(Duration::from_secs(7 * 24 * 60 * 60), 10_000)
            .with_limit(1_000),
    );
```

```rust
# use a3s_lane::{InMemoryJobQueue, JobOptions, JobQueueBackend};
# async fn events_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("email");
let job = queue
    .add_job("send".to_string(), serde_json::json!({ "to": "ops@example.com" }), JobOptions::new())
    .await?;

let events = queue.read_events("-", "+", 100).await?;
assert_eq!(events[0].event, "added");
assert_eq!(events[0].job_id.as_deref(), Some(job.id.as_str()));

queue.trim_events(10_000).await?;
# Ok(())
# }
```

Every claimed job carries an opaque `lock_token`. Workers must pass that token
to `complete_job()`, `fail_job()`, `fail_job_discarding_retry()`, and
`renew_lease()`. This prevents a stale worker from completing or failing a job
after its lease expired and another worker reclaimed it. Active leased jobs
cannot be removed through the normal management API; run stalled recovery first
when a worker lease has expired.

Flow jobs create a parent job and one or more child jobs in a single operation.
The parent starts in `waiting_children`, children are claimed normally, and the
parent is released to `waiting` after every remaining child completes or is
removed. A terminal child failure fails the parent by default; retryable child
failures keep the parent blocked until the child retries and reaches a terminal
outcome. Active parent jobs can also call `add_flow_children()` with their lock
token to atomically add children and move themselves to `waiting_children`; this
is the dynamic planner/fan-out shape behind BullMQ's `moveToWaitingChildren()`.
When a submitted flow parent uses an existing custom job id, Lane follows
BullMQ's `addParentJob` duplicate path: the stored parent data is kept,
`duplicated` is emitted for the parent id, and the submitted children are still
added, attached, deduplicated, or skipped according to the normal child rules.
When a dynamically added child uses an existing custom job id, Lane keeps the
existing child data, emits `duplicated`, updates `parent_id`, records a pending
dependency for non-completed children, and lets completed children satisfy the
dependency immediately.
Dynamic children follow the same BullMQ deduplication path as static flow
children. A child candidate that matches an existing deduplication owner is
skipped, emits `debounced` and `deduplicated` on the owner id, and is not
attached to the active parent. If the matching owner is active and the candidate
uses `keep_last_if_active(true)`, Lane stores the latest candidate as the next
child for that parent; the parent stays in `waiting_children` until the owner
finalizes and the next child materializes.
Optional children can use
`JobOptions::new().with_ignore_dependency_on_failure(true)` to mirror BullMQ's
`ignoreDependencyOnFailure`: terminal failure removes that child from the
parent's still-blocking dependency set, counts it as ignored, and releases the
parent once the remaining dependencies finish.
`JobOptions::new().with_remove_dependency_on_failure(true)` mirrors BullMQ's
`removeDependencyOnFailure`: terminal failure also removes the child from the
still-blocking dependency set, but does not add it to the ignored dependency
count.
`JobOptions::new().with_continue_parent_on_failure(true)` mirrors BullMQ's
`continueParentOnFailure`: terminal failure removes the child from the
still-blocking dependency set, records the failure for parent inspection, and
moves the parent to `waiting` or `delayed` immediately instead of waiting for the
remaining dependencies.
`JobOptions::new().with_fail_parent_on_failure(true)` mirrors BullMQ's
`failParentOnFailure`: terminal failure removes the child from the
still-blocking dependency set, releases the parent early with a deferred failure,
and lets the worker fail the parent before running the parent processor.
Parents can call `get_flow_children_values()` after fan-in release to retrieve
completed child return values, mirroring BullMQ's `getChildrenValues()`.
`get_flow_ignored_children_failures()` mirrors BullMQ's
`getIgnoredChildrenFailures()` and returns failures from children configured with
`ignoreDependencyOnFailure` or `continueParentOnFailure`; removed dependency
failures are intentionally omitted.

```rust
use a3s_lane::{
    InMemoryJobQueue, JobFlowDependencyCountOptions, JobFlowDependencyKind,
    JobFlowDependencyPageCursor, JobFlowDependencyPageOptions, JobFlowDependencyPagesOptions,
    JobOptions, JobSpec, JobState,
};

# async fn flow_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("reports");

let flow = queue
    .add_flow(
        JobSpec::new("aggregate", serde_json::json!({ "report": "daily" }))
            .with_options(JobOptions::new().with_priority(1)),
        vec![
            JobSpec::new("fetch-us", serde_json::json!({ "region": "us" })),
            JobSpec::new("fetch-eu", serde_json::json!({ "region": "eu" })),
        ],
    )
    .await?;

assert_eq!(flow.parent.state, JobState::WaitingChildren);

let dependencies = queue.get_flow_dependencies(&flow.parent.id).await?.unwrap();
assert_eq!(dependencies.pending_child_ids.len(), 2);
assert!(dependencies.missing_child_ids.is_empty());

let counts = queue
    .get_flow_dependency_counts(&flow.parent.id)
    .await?
    .unwrap();
assert_eq!(counts.unprocessed, 2);

let selected_counts = queue
    .get_flow_dependency_selected_counts(
        &flow.parent.id,
        JobFlowDependencyCountOptions::new().with_unprocessed(true),
    )
    .await?
    .unwrap();
assert_eq!(selected_counts.unprocessed, Some(2));
assert_eq!(selected_counts.processed, None);

let dependency_values = queue
    .get_flow_dependency_values(&flow.parent.id)
    .await?
    .unwrap();
assert_eq!(dependency_values.unprocessed.len(), 2);

let pending_page = queue
    .get_flow_dependency_page(
        &flow.parent.id,
        JobFlowDependencyPageOptions::new(JobFlowDependencyKind::Unprocessed).with_count(20),
    )
    .await?
    .unwrap();
assert_eq!(pending_page.items.len(), 2);
assert_eq!(pending_page.next_cursor, 0);

let dependency_pages = queue
    .get_flow_dependency_pages(
        &flow.parent.id,
        JobFlowDependencyPagesOptions::new()
            .with_unprocessed(JobFlowDependencyPageCursor::new().with_count(20)),
    )
    .await?
    .unwrap();
assert_eq!(
    dependency_pages
        .get(JobFlowDependencyKind::Unprocessed)
        .unwrap()
        .items
        .len(),
    2
);

let removed = queue
    .remove_unprocessed_children(&flow.parent.id, chrono::Utc::now())
    .await?
    .unwrap();
assert_eq!(removed.len(), 2);

let detached_flow = queue
    .add_flow(
        JobSpec::new("aggregate-detach", serde_json::json!({})),
        vec![JobSpec::new("optional-child", serde_json::json!({}))],
    )
    .await?;
assert!(queue
    .remove_child_dependency(&detached_flow.children[0].id, chrono::Utc::now())
    .await?);
# Ok(())
# }
```

Repeat jobs schedule the next occurrence after a successful completion. Use
`RepeatOptions::every()` for fixed intervals or `RepeatOptions::cron()` for a
seven-field UTC cron expression. The repeat `limit` counts total executions,
including the first job. A custom repeat key also acts as a series owner: while a
non-terminal occurrence with the same repeat key exists, duplicate adds return
that owner instead of creating a parallel repeat chain. In Redis, duplicate
repeat adds can recover from a missing fast owner key by validating
`repeat_meta:<key>.jid` and restoring `repeat:<key>` before returning the
current owner. Adds, bulk adds, flow adds, dynamic flow children, and repeat
upserts reject repeat options whose `end_at` is earlier than the add timestamp,
matching BullMQ's `endDate` add-time guard and avoiding partial writes:

```rust
use a3s_lane::{InMemoryJobQueue, JobOptions, JobSpec, RepeatOptions};
use std::time::Duration;

# async fn repeat_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("sync");

let job = queue
    .add(
        "heartbeat",
        serde_json::json!({ "target": "crm" }),
        JobOptions::new().with_repeat(
            RepeatOptions::every(Duration::from_secs(60))
                .with_limit(10)
                .with_key("crm-heartbeat"),
        ),
    )
    .await?;

assert_eq!(job.repeat_key.as_deref(), Some("crm-heartbeat"));

let cron_job = queue
    .add(
        "nightly-import",
        serde_json::json!({ "target": "warehouse" }),
        JobOptions::new().with_repeat(
            RepeatOptions::cron("0 0 2 * * * *")
                .with_limit(30)
                .with_key("warehouse-nightly-import"),
        ),
    )
    .await?;

assert_eq!(
    cron_job.repeat_key.as_deref(),
    Some("warehouse-nightly-import")
);

let duplicate = queue
    .add(
        "heartbeat",
        serde_json::json!({ "target": "crm", "duplicate": true }),
        JobOptions::new().with_repeat(
            RepeatOptions::every(Duration::from_secs(60))
                .with_limit(10)
                .with_key("crm-heartbeat"),
        ),
    )
    .await?;

assert_eq!(duplicate.id, job.id);

let replacement = queue
    .upsert_repeat(
        JobSpec::new(
            "heartbeat-v2",
            serde_json::json!({ "target": "crm", "template": "v2" }),
        )
        .with_options(
            JobOptions::new().with_repeat(
                RepeatOptions::every(Duration::from_secs(30))
                    .with_limit(10)
                    .with_key("crm-heartbeat"),
            ),
        ),
        chrono::Utc::now(),
    )
    .await?;

assert_ne!(replacement.id, job.id);

let repeats = queue.list_repeats().await?;
assert_eq!(repeats[0].key, "crm-heartbeat");
assert_eq!(repeats[0].job_id, replacement.id);

let removed = queue.remove_repeat("crm-heartbeat").await?;
assert_eq!(
    removed.as_ref().map(|job| job.id.as_str()),
    Some(replacement.id.as_str())
);
# Ok(())
# }
```

Simple deduplication coalesces duplicate submissions while the first matching
job owns its deduplication id. An optional TTL limits how long that owner key
blocks duplicates, including when the owner has already completed or failed:

```rust
use a3s_lane::{DeduplicationOptions, InMemoryJobQueue, JobOptions, JobQueueBackend};
use chrono::Utc;
use std::time::Duration;

# async fn dedup_example() -> a3s_lane::Result<()> {
let queue = InMemoryJobQueue::new("billing");

let first = queue
    .add(
        "recalculate-account",
        serde_json::json!({ "account_id": "acct_42" }),
        JobOptions::new().with_deduplication_id("account:acct_42"),
    )
    .await?;

let duplicate = queue
    .add(
        "recalculate-account",
        serde_json::json!({ "account_id": "acct_42", "duplicate": true }),
        JobOptions::new().with_deduplication_id("account:acct_42"),
    )
    .await?;

assert_eq!(duplicate.id, first.id);

let ttl_owner = queue
    .add(
        "refresh-account",
        serde_json::json!({ "account_id": "acct_42" }),
        JobOptions::new().with_deduplication(
            DeduplicationOptions::new("account-refresh:acct_42")
                .with_ttl(Duration::from_secs(30))
                .extend_ttl(true),
        ),
    )
    .await?;

assert!(ttl_owner.deduplication_expires_at.is_some());

let delayed_owner = queue
    .add(
        "refresh-account",
        serde_json::json!({ "account_id": "acct_42", "version": 1 }),
        JobOptions::new()
            .with_delay(Duration::from_secs(60))
            .with_deduplication(
                DeduplicationOptions::new("account-refresh:acct_42")
                    .replace_delayed(true),
            ),
    )
    .await?;

let replacement = queue
    .add(
        "refresh-account",
        serde_json::json!({ "account_id": "acct_42", "version": 2 }),
        JobOptions::new()
            .with_delay(Duration::from_secs(60))
            .with_deduplication(
                DeduplicationOptions::new("account-refresh:acct_42")
                    .replace_delayed(true),
            ),
    )
    .await?;

assert_ne!(replacement.id, delayed_owner.id);

let active_owner = queue
    .add(
        "sync-account",
        serde_json::json!({ "account_id": "acct_42", "version": 1 }),
        JobOptions::new().with_deduplication(
            DeduplicationOptions::new("account-sync:acct_42")
                .keep_last_if_active(true),
        ),
    )
    .await?;
let claimed = queue
    .claim_next("worker-a".to_string(), Duration::from_secs(30), Utc::now())
    .await?
    .expect("job should be claimable");

let duplicate = queue
    .add(
        "sync-account",
        serde_json::json!({ "account_id": "acct_42", "version": 2 }),
        JobOptions::new().with_deduplication(
            DeduplicationOptions::new("account-sync:acct_42")
                .keep_last_if_active(true),
        ),
    )
    .await?;

assert_eq!(duplicate.id, active_owner.id);
queue
    .complete_job(
        &claimed.id,
        claimed.lock_token.as_deref().expect("claimed jobs have locks"),
        serde_json::json!({ "ok": true }),
        Utc::now(),
    )
    .await?;
# Ok(())
# }
```

The current deduplication mode intentionally covers BullMQ's simple mode. A
deduplication id without a TTL blocks duplicate adds until the owning job
completes, fails terminally, is removed, or is cleaned. A TTL-backed
deduplication id follows BullMQ's Redis finalization rule: completion and
terminal failure keep the owner key while its Redis TTL is still positive, so
duplicates continue to return the retained terminal owner until the TTL expires
when that terminal job record is retained. When `remove_on_complete(true)`,
`remove_on_fail(true)`, or finished-job retention deletes the job record in the
same move-to-finished turn, the Redis deduplication key is still left to expire
like BullMQ's Lua path, but Lane's high-level add/get APIs require a usable job
snapshot and may prune a missing owner before accepting a later replacement.
Removal-style paths such as explicit remove, clean, drain, and manual
`remove_deduplication_key()` clear the owner immediately.
`extend_ttl(true)` covers BullMQ's debounce extension path: duplicate adds
return the current owner and refresh the deduplication TTL instead of allowing
the owner key to expire at the original deadline.
`replace_delayed(true)` also covers BullMQ's delayed-owner replace path: a new
deduplicated add may remove a delayed standalone owner and insert the new job in
the same operation when the old owner is still present in the delayed index.
For TTL-backed delayed replacement, replacement preserves the existing owner
key's remaining TTL by default; when `extend_ttl(true)` is also set, replacement
refreshes the TTL instead.
`keep_last_if_active(true)` covers BullMQ's active-owner keep-last path for
standalone and repeat-series jobs: duplicates added while the current owner is
active return that owner, overwrite a queue-local next-job record, and
materialize only the latest duplicate when the owner completes, terminally fails,
or exhausts stalled-job recovery. If that latest duplicate has a delay, the delay
starts from the owner finalization timestamp. For repeat series, the latest
duplicate becomes the next occurrence for the same repeat key and replaces the
regular successor for that finalization turn. For flow parents in the
in-memory/local runtime, a duplicate flow submitted while the parent owner is
active stores the latest replacement parent and children, then materializes that
flow when the active parent finalizes. Redis Lua now covers active parent
completion, terminal failure, and stalled terminal-failure paths for flow
keep-last.
Ordinary flow child deduplication follows BullMQ's child add path for both
static `add_flow()` and dynamic `add_flow_children()`: if a child candidate
matches an existing deduplication owner, Lane returns and emits events for the
owner, skips storing the candidate child, leaves the owner detached from the new
parent, and records only the non-skipped children as the new parent's pending
dependencies. When that child deduplication uses `keep_last_if_active` and the
owner is active, Lane stores the latest candidate as the next child instead. The
active owner still owns the deduplication id, and owner finalization materializes
the latest child and registers it as a dependency of the candidate parent.
Retrying a failed deduplicated job reclaims the deduplication id while the job is
waiting or active again; retry is rejected if another live deduplication owner,
including a retained terminal TTL owner, already owns that id.
`remove_deduplication_key()` clears the queue's current owner for a
deduplication id before finalization, or during a retained terminal TTL window,
matching BullMQ's queue-level `removeDeduplicationKey()` behavior of deleting
the Redis deduplication key. The original job remains in its current state, but
later submissions with the same deduplication id can become the new owner.
`get_deduplication_job_id()` returns the current usable owner job id for that
deduplication id; the Redis backend validates the owner job snapshot instead of
blindly exposing an orphaned raw key.

Use `LocalJobQueue` when a process-local runtime needs durable restart
recovery. Its JSON snapshot stores jobs, events, deduplication follow-up jobs,
released deduplication owners, and parent-scoped flow dependency side indexes
so terminal child return values and ignored/fail-parent failure markers survive
ordinary child cleanup and process restart:

```rust
use a3s_lane::{JobOptions, JobQueueBackend, LocalJobQueue};
use std::path::PathBuf;

# async fn durable_example() -> a3s_lane::Result<()> {
let queue = LocalJobQueue::open("email", PathBuf::from("./lane-jobs/email.json")).await?;
let job = queue
    .add(
        "send",
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new().with_priority(10),
    )
    .await?;

let claimed = queue
    .claim_next("worker-1".to_string(), std::time::Duration::from_secs(30), chrono::Utc::now())
    .await?;

if let Some(claimed) = claimed {
    let lock_token = claimed
        .lock_token
        .as_deref()
        .expect("claimed jobs include a lock token");
    queue
        .complete_job(
            &claimed.id,
            lock_token,
            serde_json::json!({ "ok": true }),
            chrono::Utc::now(),
        )
        .await?;
}
# Ok(())
# }
```

Use `RedisJobQueue` when multiple workers or processes need to claim from the
same durable priority queue. It stores jobs as JSON in a Redis hash, indexes
states with sorted sets, stores retained job logs in per-job Redis lists, and
uses Lua scripts to atomically add jobs, promote due delayed jobs, claim work,
and transition leased jobs. The Redis backend follows the core BullMQ locking
mechanism: a claim creates an independent TTL lock key for the job, and
complete, fail, release, delay, and renew operations must prove ownership by
matching the lock token before the script mutates the
active/completed/failed/delayed indexes. Active `get_job()` snapshots read that
lock key back so management callers can inspect the current lease token.
`renew_leases()` mirrors BullMQ's
`extendLocks` shape: Redis checks every token in one Lua turn, renews valid lock
keys, updates active lease scores and retained job snapshots, removes successful
jobs from the `stalled` candidate set, and returns only the failed job ids.
Stalled recovery uses BullMQ's two-phase candidate set shape: a recovery pass
records active jobs in a `stalled` set for the next pass, successful
renew/finalize scripts remove the job from that set, and only a later pass whose
candidate has no TTL lock can requeue or fail the job:

```rust
use a3s_lane::{JobOptions, JobQueueBackend, JobRateLimit, RedisJobQueue, RetryPolicy};
use std::time::Duration;

# async fn redis_example() -> a3s_lane::Result<()> {
let queue = RedisJobQueue::with_namespace(
    "redis://127.0.0.1/",
    "a3s:lane",
    "email",
)?;
queue.set_claim_rate_limit(JobRateLimit::new(100, Duration::from_secs(60))).await?;
assert_eq!(
    queue.get_claim_rate_limit().await?,
    Some(JobRateLimit::new(100, Duration::from_secs(60)))
);
let _rate_limit_ttl_ms = queue.get_claim_rate_limit_ttl(None).await?;
queue.rate_limit_claims_for(Duration::from_millis(500)).await?;
queue.clear_claim_rate_limit_key().await?;
queue.set_max_active_jobs(32).await?;
assert_eq!(queue.get_max_active_jobs().await?, Some(32));
assert!(!queue.is_maxed().await?);

let job = queue
    .add_job(
        "send".to_string(),
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new()
            .with_priority(10)
            .with_retry_policy(RetryPolicy::fixed(3, Duration::from_secs(1))),
    )
    .await?;

if let Some(claimed) = queue
    .claim_next_blocking(
        "worker-1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(10),
    )
    .await?
{
    let lock_token = claimed
        .lock_token
        .as_deref()
        .expect("claimed jobs include a lock token");
    queue
        .complete_job(
            &claimed.id,
            lock_token,
            serde_json::json!({ "ok": true }),
            chrono::Utc::now(),
        )
        .await?;
}

assert_eq!(queue.get_job(&job.id).await?.map(|job| job.name), Some("send".to_string()));
# Ok(())
# }
```

`with_claim_rate_limit()` configures a worker-local claim rate limit while
sharing the counter key through Redis for workers that use the same namespace
and queue. `set_claim_rate_limit()` stores the shared configuration in the queue
meta hash as `max` and `duration`, matching BullMQ's global rate-limit
mechanism. `get_claim_rate_limit()` reads those fields with `HMGET`, and
`get_claim_rate_limit_ttl()` follows BullMQ's `getRateLimitTtl` script shape:
with an explicit max it returns a TTL only after the limiter counter reaches
that threshold, otherwise it uses Redis-shared `meta.max` when present and falls
back to raw `PTTL` for the limiter key. `rate_limit_claims_for()` mirrors
BullMQ's manual `rateLimit()` path by setting the limiter key to a very large
counter with a millisecond TTL; `clear_claim_rate_limit_key()` mirrors
`removeRateLimitKey()` by deleting that limiter key without changing shared
configuration. `clear_claim_rate_limit()` removes the shared config fields. The
Lua claim script prefers an explicit worker-local limit and otherwise reads the
Redis meta values before checking the rate-limit counter. When the window is
exhausted, `claim_next()` returns `None` and the job remains waiting for a later
poll. `claim_next_blocking()` mirrors BullMQ's worker-side limiter delay by
checking the active limiter TTL after an empty claim and sleeping until the
limiter window can admit another job, capped by the worker's blocking deadline.

`set_max_active_jobs()` configures a Redis-shared active job ceiling for the
queue. It stores the value in the queue meta hash as `concurrency`, matching
BullMQ's queue-maxed mechanism. `get_max_active_jobs()` reads that same meta
field, mirroring BullMQ's global concurrency getter. `is_maxed()` mirrors
BullMQ's `isMaxed()` queue getter by reading `meta.concurrency` and the active
sorted-set count in one Lua turn. The Lua claim script reads the meta value,
checks the active sorted set count in the same Redis turn, and returns `None`
without moving a job or consuming rate-limit capacity when the queue is already
maxed. `clear_max_active_jobs()` removes the shared ceiling.

Like BullMQ's `moveToActive` script, Redis claims also promote due delayed jobs
inside the same Lua script before checking pause, rate-limit, max-active, and
the next claim. A paused or maxed queue can still move due delayed jobs back to
`waiting`; it simply returns `None` instead of leasing work. In that paused or
maxed branch, Lane suppresses the base worker marker just like BullMQ's
`addBaseMarkerIfNeeded(markerKey, isPausedOrMaxed)` helper, so delayed promotion
does not wake another worker until the queue resumes or an active slot opens.
Claiming also validates the stored job state before moving a waiting-index entry
to `active`, pruning stale waiting sorted-set entries instead of reactivating
jobs that have already moved elsewhere.

Redis also maintains a BullMQ-style queue `marker` zset inside the same Lua
state transitions that move jobs into `waiting` or `delayed`. Waiting writes add
member `0` at score `0`; delayed writes and delayed removals refresh member `1`
to the earliest delayed score, mirroring BullMQ's `addBaseMarkerIfNeeded` and
`addDelayMarkerIfNeeded` wake-up mechanism. `claim_next_blocking()` uses a
dedicated Redis connection to `BZPOPMIN` that marker set, treats the popped
marker only as a wake-up signal, and then reruns the normal Lua claim path so
pause, rate-limit, max-active, delayed promotion, and lock ownership checks stay
atomic. A successful claim rewrites the base marker to fan out multiple blocked
workers over bulk-added jobs, and pause/resume updates the marker set so resumed
queues wake sleeping Redis workers. Active-job finalization paths also refresh
the base marker whenever waiting work remains, so completing, terminally
failing, retry-delaying, or manually delaying a leased job wakes blocked Redis
workers after a `set_max_active_jobs()` slot becomes available.
`JobQueueBackend::claim_next_blocking()` exposes that wait path to the
backend-agnostic `JobWorker`; non-blocking backends use the default immediate
`claim_next()` fallback, while Redis workers use the marker-backed `BZPOPMIN`
path when the queue is not currently rate-limited.

Redis adds are Lua-backed as well. The add scripts write job JSON and the
waiting, delayed, or waiting-children index in the same Redis turn. If a custom
job id already exists, the script returns the existing job without advancing the
waiting sequence or writing duplicate state indexes. Lane rejects custom job ids
equal to `0`, prefixed with `0:`, or pure integers before script execution,
matching BullMQ's reserved marker namespace and integer-id guard. Redis scripts
special-case marker-like values while claiming, listing, and promoting jobs, so
Lane keeps those ids out of the user job-id namespace. Bulk add follows the same
mechanism in one script call while preserving the caller's input order, including
the same deduplication stream events that BullMQ's pipelined `addBulk()` emits
for each job.
For simple deduplication, the same add scripts use an independent
`deduplication:<id>` key, equivalent to BullMQ's `de:<id>` role, to return the
current owner before writing a duplicate. If `DeduplicationOptions` has a TTL,
the Lua scripts write that owner key with `PX` so Redis expires the
deduplication window even if the original job later completes or fails before
the TTL does. Completion, terminal failure, and stalled terminal-failure scripts
mirror BullMQ's `removeDeduplicationKeyIfNeededOnFinalization`: they delete a
matching owner key only when Redis reports no TTL (`PTTL == -1`) or an expiring
zero TTL, and preserve keys with a positive TTL. The keep-last-if-active mode
intentionally omits that TTL, matching BullMQ's active owner behavior so the key
cannot expire while work is still leased. If
`extend_ttl(true)` is set, duplicate adds refresh the owner key with `PX` before
returning the current owner, matching BullMQ's debounce extension branch.
If `replace_delayed(true)` is set and the current owner is a standalone delayed
job, the add script first removes the old delayed zset member, then removes the
old job hash and inserts the new owner only if that delayed removal succeeded,
mirroring BullMQ's delayed replacement branch. With TTL-backed deduplication, the
script updates the owner id with Redis `KEEPTTL` so replacement does not extend
the remaining deduplication window unless `extend_ttl(true)` is also set. That
same branch emits BullMQ-style `removed prev=delayed`, `debounced`, and
`deduplicated` events before the replacement job's own add/state events. If
`keep_last_if_active(true)` is set and the current owner is present in the
active sorted set, duplicate adds overwrite a
`deduplication_next:<id>` proto-job record and `PERSIST` the owner key. For
standalone and repeat jobs, complete, terminal fail, and stalled terminal-fail
scripts then atomically delete the old owner key, materialize that latest
proto-job into waiting or delayed state, and set the deduplication owner to the
new job. When the owner and latest duplicate
share the same repeat key, the finalization script also increments
`repeat_count`, sets the `repeat:<key>` owner to the materialized latest job, and
suppresses the regular repeat successor for that turn. This preserves the
single-owner repeat invariant while matching BullMQ's keep-last requeue
mechanism, where the dedup-next record is consumed during job finalization rather
than by a later client-side pass. Flow keep-last uses the same
`deduplication_next:<id>` key with a flow envelope; Redis currently materializes
that envelope on active parent completion, terminal failure, or stalled terminal
failure. Active flow-child keep-last deduplication stores the next child with its
parent relationship and registers the materialized child in the parent dependency
set when the active owner finalizes. Flow parent deduplication follows BullMQ's
`addParentJob` path too:
duplicate parent submissions return the current owner flow and write
`debounced` and `deduplicated` events on the owner parent id; active keep-last
flow duplicates write the same events while replacing the pending
`deduplication_next:<id>` flow envelope. Redis removal paths mirror BullMQ's
removal helper too: when remove,
clean, drain, repeat upsert, or flow unprocessed-child removal deletes the job
that still owns `deduplication:<id>`, it also clears `deduplication_next:<id>` so
a previously active owner cannot leave a stale shadow job behind.

Waiting order is modeled after BullMQ's Redis-level mechanism rather than only
matching its option names. In BullMQ 5.79.3, standard jobs use a Redis list:
`opts.lifo` selects `RPUSH`, FIFO uses `LPUSH`, and workers consume from the
tail with `RPOPLPUSH`; prioritized jobs use a sorted set whose score is
`priority * 0x100000000 + counter`, and `changePriority(..., lifo: true)` puts
the job at the front of its same-priority score range. Lane stores all waiting
jobs in one sorted set, so each Lua script that moves a job into `waiting`
increments the queue sequence, writes that value to `job.enqueued_seq`, and
computes a priority-bucketed score. The lower half of each priority bucket is
reserved for LIFO entries with reversed sequence order, and the upper half is
reserved for FIFO entries with forward sequence order. This keeps `ZRANGE`
claiming priority-first, newest LIFO before older LIFO, LIFO before FIFO at the
same priority, and oldest FIFO before newer FIFO, while preserving
`get_counts_per_priority()` as a `ZCOUNT` over the same priority bucket.
`release_active_job()` writes the returned job at the start of its priority
bucket, mirroring BullMQ's `pushBackJobWithPriority()` score for prioritized
jobs and the `RPUSH` front-of-consumption behavior for standard wait-list jobs;
if multiple released jobs share that exact score, Redis orders them by job id.

Finished-job retention follows BullMQ's underlying `moveToFinished` mechanism
rather than only matching the `removeOnComplete` and `removeOnFail` option
names. In BullMQ 5.79.3, those options are normalized to `keepJobs`; `true`
becomes `{ count: 0 }`, `false` becomes unlimited retention, a number becomes
`{ count: number }`, and an object may carry `age`, `count`, and `limit`. The
`moveToFinished-14.lua` script writes the current job to the completed or failed
zset with the finish timestamp as score, then calls
`removeJobsByMaxAge(timestamp, maxAge, targetSet, prefix, maxLimit)` and
`removeJobsByMaxCount(maxCount, targetSet, prefix)` in the same Lua turn.
Lane mirrors that storage-level behavior: Redis completion, terminal failure,
stalled terminal-failure, and flow-cleanup scripts that fail a parent first
finalize the job, then apply age cleanup, then count cleanup against the
terminal zset while deleting the job hash, log list, and dependency set for
removed finished jobs. Like BullMQ's `moveToFinished` scripts, this finished-job
record cleanup does not delete the deduplication owner key; a TTL-backed owner
continues to live until Redis expires it, while a no-TTL owner is already
released during finalization.
In-memory and local durable queues use the same order against `finished_at`
timestamps. Age cleanup is best-effort just like BullMQ: there is no background
timer, so an over-age completed or failed job is removed only when a later job
enters the same terminal state.

Queue events follow BullMQ's Redis stream mechanism. BullMQ's Lua scripts write
global queue events with `XADD <queue>:events`, commonly using
`MAXLEN ~ maxEvents` with a default of 10,000 retained entries; `QueueEvents`
then reads from that stream by event id. Lane mirrors that storage shape for the
Redis backend with an `events` stream per queue. Lua state transitions write the
event in the same Redis turn as the job mutation: add writes `added` followed by
`waiting`, `delayed`, or `waiting-children`; claim writes `active prev=waiting`;
completion writes `completed prev=active` with `returnvalue`; failure writes
`failed` or retry `delayed` with `failedReason`, and terminal failures whose
attempt count is exhausted also write BullMQ-style `retries-exhausted` with
`attemptsMade`; completed and terminal failed move-to-finished paths write a
queue-level `drained` event when no waiting or active jobs remain; flow child
completion, terminal failure, and stalled terminal failure paths also emit parent
`waiting`, `delayed`, or `failed` events with `prev=waiting-children` when that
same Lua turn releases or fails the parent;
explicit removal writes `removed prev=<state>` for the removed job; `clean_jobs()`
writes a queue-level `cleaned count=<n>` event after removing aged jobs;
deduplicated adds, including bulk adds, write BullMQ-style `debounced` and
`deduplicated` events with the owner job id, deduplication id, and skipped
candidate job id; flow parent deduplication writes the same event pair with the
owner parent id and skipped candidate parent id; ordinary flow child
deduplication writes the event pair on the existing child owner while omitting the
skipped candidate from the new parent dependency set; flow child custom job-id
duplicates write BullMQ-style `duplicated` on the retained child id when the
existing child is attached to the new parent; delayed-owner replacement also
writes `removed prev=delayed` for the old owner followed by `debounced` and
`deduplicated` events on the replacement job id; progress writes
`progress data=<json>`; pause/resume write queue-level events.
`read_events()` uses `XRANGE` over stream ids, and
`trim_events()` uses BullMQ-style `XTRIM MAXLEN ~`. The in-memory and local
durable backends keep the same retained event entries in their snapshots so
tests and embedded runtimes expose the same contract without Redis. Like
BullMQ's `addLog` script, Lane job logs remain a retained log list and do not
emit queue events; progress updates do.

Completion, terminal failure, and stalled terminal failure scripts use
BullMQ-style finalization semantics for deduplication keys: a matching owner key
with no TTL is released, while a matching key with a positive TTL remains until
Redis expires it, even if `remove_on_complete(true)`, `remove_on_fail(true)`, or
finished-job retention deletes the finished job record immediately. Remove,
clean, drain, repeat upsert, and flow child-removal paths use removal semantics
instead: they release the matching owner key and also clear the paired
`deduplication_next:<id>` shadow record, matching BullMQ's removal cleanup for
keep-last deduplication.
Manual retry reclaims the key inside the retry script, reapplies the TTL, and
refuses to move the failed job back to waiting if a newer non-terminal job
already owns the same deduplication id.
`remove_deduplication_key()` deletes `deduplication:<id>` directly, so a later
add can claim the same id even while the old owner remains non-terminal. When a
keep-last owner has a pending successor, the release also clears
`deduplication_next:<id>` so the old active owner cannot materialize a stale
duplicate after the id was manually released. The in-memory and local durable
backends persist the same logical release by tracking the released owner id in
their snapshots instead of relying on a client-side scan alone.
`get_deduplication_job_id()` consults that same `deduplication:<id>` key. Unlike
BullMQ's raw `GET de:<id>` getter, Lane validates that the owner can still be
loaded as a job snapshot for the job-returning API surface; if the key points at
a missing or mismatched job, or at a terminal job without a positive TTL owner
key, it clears both the stale owner key and any orphaned
`deduplication_next:<id>` record before reporting no owner. Terminal jobs with a
positive TTL and a retained job record remain valid deduplication owners until
Redis expires the key.

Redis flow submission is all-or-nothing: the flow add script writes the parent,
new children, existing-parent and existing-child attachments, state indexes,
queue events, and the parent's pending dependency set in one Redis turn.
Duplicate ids inside the same submitted flow are rejected. An existing parent
custom job id follows BullMQ's `addParentJob` duplicate path: Lane keeps the
stored parent snapshot, emits `duplicated`, preserves the current dependency set,
and adds only the submitted children that are new, duplicated for that parent, or
deduplication keep-last placeholders. An existing child custom job id follows
BullMQ's `handleDuplicatedJob` path when it has no conflicting retained parent:
Lane keeps the original child data, updates its `parent_id`, emits `duplicated`,
adds non-completed children to the new parent dependency set, and lets an
already completed child satisfy the dependency immediately so the parent can
leave `waiting_children` in the same turn. If the existing child still belongs
to a different retained parent, the flow add returns a parent-conflict error
without creating partial records. If a child candidate
deduplicates against an existing owner, the add script handles that before
dependency insertion: ordinary candidates are skipped, the existing owner is not
attached to the new parent, and the returned flow contains only the children that
were actually stored. Active keep-last child candidates are stored in
`deduplication_next:<id>` with their parent id; when the owner finalizes, the
materialized child is added to the parent dependency set in the same Redis turn.
`get_flow_dependencies()` uses a Redis-side read script to load the parent and
every retained child snapshot from the jobs hash in one turn, and returns the
child ids that are still pending or missing from retention.
`get_flow_dependency_counts()` follows BullMQ's `getDependencyCounts` Redis/Lua
mechanism instead of only copying the API names. BullMQ 5.79.3 counts
parent-scoped `:processed`, `:dependencies`, `:failed`, and `:unsuccessful`
structures with `HLEN`, `SCARD`, `HLEN`, and `ZCARD`, with ignored, removed, and
continued failures handled by the failure-policy path. Lane now writes the same
parent-scoped Redis side indexes under `dependencies:<parent_id>:processed`,
`dependencies:<parent_id>:failed`, and
`dependencies:<parent_id>:unsuccessful`, while in-memory and local-durable
queues keep equivalent `JobQueueSnapshot.flow_dependency_indexes` entries.
Child snapshots remain available for audit and compatibility fallback. The
Redis count script reads those side indexes in one turn and returns processed,
unprocessed, failed, ignored, and missing totals without returning every child
snapshot to the client; the in-memory/local readers use the same authoritative
side-index-first view and fall back to retained child snapshots only for child
ids not covered by a side index.
Removed failed dependencies are intentionally omitted from the failed and ignored
totals, matching BullMQ's `removeDependencyOnFailure` behavior.
`get_flow_dependency_selected_counts(parent_id, options)` mirrors BullMQ's
`Job.getDependenciesCount(opts)` selector semantics. Empty options default to
the four BullMQ buckets, while explicit options return `Some(count)` only for
requested processed, unprocessed, ignored, and failed buckets. Redis reads the
same parent-scoped side indexes with `HLEN`, `SCARD`, `HLEN`, and `ZCARD`,
matching BullMQ's `getDependencyCounts-4.lua` mechanism and avoiding snapshot
fan-out when callers only need counts. Lane keeps `get_flow_dependency_counts()`
as the extended queue-level count snapshot with `missing` and compatibility
fallback support.
`get_flow_dependency_values(parent_id)` mirrors BullMQ's no-options
`Job.getDependencies()` path: Redis reads the same parent-scoped `:processed`,
`:dependencies`, `:failed`, and `:unsuccessful` structures with `HGETALL`,
`SMEMBERS`, `HGETALL`, and `ZRANGE 0 -1`, parsing processed values as JSON and
ignored values as failure-reason strings. It then merges retained child
snapshots for any parent child id not covered by those side indexes, preserving
full-bucket compatibility for flows created before the side-index fields were
available.
`get_flow_dependency_page(parent_id, options)` and
`get_flow_dependency_pages(parent_id, options)` mirror BullMQ's paginated
`Job.getDependencies(opts)` path for large fan-out inspection. Redis reads
`processed` with `HSCAN dependencies:<parent_id>:processed`, `unprocessed` with
`SSCAN dependencies:<parent_id>`, `ignored` with
`HSCAN dependencies:<parent_id>:failed`, and `failed` with
`ZRANGE dependencies:<parent_id>:unsuccessful`. The multi-bucket getter keeps
the BullMQ result order and reads all requested buckets in one Lua turn instead
of stitching together multiple client round trips. The `count` option is a Redis
scan hint for hash and set buckets, just like BullMQ; callers should keep reading
with the returned cursor until it becomes `0`. For mixed upgrade data, the
initial `cursor = 0` page also appends retained child snapshot fallback entries
that are not covered by side indexes; later cursor pages remain pure Redis
cursor scans.
When a child completes, fails with `ignore_dependency_on_failure` or
`continue_parent_on_failure`, fails with `fail_parent_on_failure`, or when a
static flow or active parent fan-out reuses an existing completed child by custom
id, Lane mirrors BullMQ's parent-scoped side-index path instead of only relying
on the child snapshot fallback. Mixed flows with reused completed children,
newly completed children, ignored failures, and fail-parent failures therefore
read one authoritative dependency view across Redis, in-memory, and local
durable backends.
Completing a flow parent checks both the Redis dependency set and
`dependencies:<parent_id>:unsuccessful` before leaving the active state, matching
BullMQ's `moveToFinished` guard that rejects jobs with pending dependencies or
unsuccessful child dependencies. When `continueParentOnFailure` releases a
parent early, later child completion still removes that child from the dependency
set so the parent can only finish after the remaining required fan-in has
resolved.
`get_flow_children_values()` and `get_flow_ignored_children_failures()` follow
BullMQ's `getChildrenValues()` and `getIgnoredChildrenFailures()` fan-in
semantics. BullMQ reads parent-scoped `:processed` and `:failed` hashes; Lane's
Redis read scripts now prefer those hashes as well, then merge retained child
snapshots for any child id not covered by the side index. This preserves
compatibility for mixed upgrade data where some children completed before the
side indexes existed and later children wrote the new parent-scoped hashes.
Completed children whose return value is JSON `null` remain visible through both
side-index reads and retained-snapshot fallback reads.
`remove_unprocessed_children()` follows BullMQ's `removeUnprocessedChildren`
script shape at the dependency-set level: it removes children that are still in
the parent's pending dependency set, skips completed, failed, active, or locked
children, deletes the removed child records and per-child metadata, emits a
BullMQ-style `removed` event for each removed child in the same Redis turn, then
checks whether the parent can leave `waiting_children`. Lane returns the removed
child snapshots for auditability while preserving the parent `child_ids`, so
later dependency inspection reports removed children as missing.
`remove_child_dependency()` follows BullMQ's `removeChildDependency` path: it
removes one child from the parent's pending dependency set when present, clears
the child's parent reference, keeps the child job itself, and releases the
parent when no pending dependencies remain. Redis treats the pending dependency
set, parent `child_ids`, and parent-scoped `:processed`, `:failed`, and
`:unsuccessful` buckets as relationship evidence, so terminal children that have
already left the pending set can still be detached without leaving ghost
dependency values behind. In-memory and local-durable queues expose the same
visible detach semantics by allowing retained completed or failed child
snapshots to be removed from the parent's `child_ids` without deleting the child
job. A stale dependency entry for an already terminal child is still removed
just like BullMQ's `SREM` path. Ordinary job removal, clean, and drain paths
remain separate from explicit dependency detach: they follow BullMQ's
`removeJob`, `cleanJobsInSet`, and `drain` scripts by releasing pending parent
dependencies and deleting the removed job's own metadata, while retained
parent-scoped terminal dependency result indexes are not treated as cleanup
targets outside explicit dependency detach. Local durable snapshots persist the
same distinction, so reopening a queue after a completed or ignored child job was
removed still preserves the parent's processed or ignored dependency bucket,
while `remove_child_dependency()` clears the bucket intentionally.

Flow fan-in is also protected in Redis transitions. Redis flow submission writes
a pending dependency set for the parent, and child completion, removal, and
cleanup scripts remove the child id from that set before checking whether the
parent can be released to `waiting`, parked in `delayed` until its own schedule
is due, or failed because a child reached terminal failure. This follows
BullMQ's dependency-removal mechanism: cleanup that removes a child also updates
the parent dependency state instead of relying on a later client-side cleanup
pass.
Dynamic flow fan-out is Redis-atomic as well: `add_flow_children()` checks the
parent lock and rejects parents whose `dependencies:<parent_id>:unsuccessful`
zset is non-empty before inserting new dependencies, matching BullMQ's
`moveToWaitingChildren` failed-child guard. It also falls back to retained child
snapshots for mixed upgrade data where the side index is missing but a failed
dependency is still recorded. When the guard passes it inserts new children or
attaches existing custom-id children, skips ordinary deduplicated child
candidates, stores active keep-last child candidates in `deduplication_next:<id>`,
updates `dependencies:<parent_id>`, removes the parent from `active`, deletes
its lock, writes the parent into
`waiting_children`, and releases it immediately when all attached children were
already completed in one Lua script. Keep-last placeholders keep the parent
blocked until the owner finalization script materializes the latest child.
`ignore_dependency_on_failure`, `remove_dependency_on_failure`,
`continue_parent_on_failure`, and `fail_parent_on_failure` use Redis-side
failure-policy paths for terminal `fail_job()` and stalled terminal failure.
Ignored and removed failures remove the failed child from
`dependencies:<parent_id>` and release or delay the parent only when the
remaining dependency set is empty. Continued failures remove the failed child and
move the parent to `waiting` or `delayed` immediately, leaving other pending
dependencies inspectable. Fail-parent failures remove the failed child, write the
child id into `dependencies:<parent_id>:unsuccessful`, keep the remaining
dependencies inspectable, store a deferred failure on the parent, and let the
worker fail the parent before processor execution, matching BullMQ's `fpof` plus
`defa` path. Retrying that child removes the unsuccessful entry and restores the
parent dependency set. If a failure policy has already released the parent,
later terminal child failures still remove their pending dependency and update
the parent-scoped failure indexes instead of remaining visible as unprocessed.
The failed child remains retained for inspection. Ignored and continued failures
are reported through the ignored dependency count; removed failures are retained
but omitted from failed and ignored dependency counts, while fail-parent failures
remain in the failed dependency count.

Repeat successors are created during the Redis completion script too. The
worker computes the next occurrence from `RepeatOptions`, then the Lua script
finishes the current job and writes the next delayed or waiting occurrence in
the same Redis turn. Redis keeps both a lightweight `repeat:<key>` owner key for
fast collision checks and a scheduler index made of the queue-level `repeat`
zset plus `repeat_meta:<key>` hashes. The add scripts check the owner key and
fall back to scheduler metadata before inserting a new repeat job, the
completion script transfers ownership and scheduler metadata to the successor
before releasing the completed occurrence, and terminal failure, remove, clean,
drain, and stalled terminal failure release both records only if they still
point at the job being finalized or removed. Those release helpers also check
`repeat_meta:<key>.jid`, so a terminal script clears scheduler metadata even when
the fast `repeat:<key>` owner key has already disappeared.
Manual retry reclaims the repeat key and scheduler metadata inside the retry
script and rejects retry if another non-terminal occurrence already owns the
series. `list_repeats()` reads the scheduler zset first, loads each owner job
snapshot from the jobs hash, returns only non-terminal matching owners, restores
the fast `repeat:<key>` owner key from `repeat_meta:<key>.jid` when that
scheduler owner is still valid, clears stale scheduler/owner records that point
at missing, terminal, or mismatched jobs, and scans legacy `repeat:<key>` owner
keys as a migration fallback.
`remove_repeat()` resolves the current `repeat:<key>` owner, falls back to the
`repeat_meta:<key>` scheduler owner id when the fast owner key is missing, and
then runs the same Redis-side removal path as `remove_job()`, so it rejects
active leased owners, removes the job hash and state indexes, releases repeat
and deduplication ownership, and can unblock flow parents. Repeat readers use
the same scheduler metadata fallback: if the fast owner key is missing but
`repeat_meta:<key>.jid` still points at a valid non-terminal repeat owner, Redis
returns that owner and restores `repeat:<key>` with `SET NX`. If the owner key
or scheduler metadata points at a missing job, Redis clears the stale owner key,
zset entry, and metadata hash only when they still describe that missing owner.
`upsert_repeat()` follows BullMQ's
`upsertJobScheduler(..., override: true)` mechanism at Lane's current
repeat-owner layer: the Redis script resolves the current `repeat:<key>` owner,
falls back to `repeat_meta:<key>.jid` when the fast owner key is missing,
repairs that owner key when the scheduler owner is still valid, rejects active
leased owners, rejects flow-owned occurrences to avoid corrupting parent
dependencies, checks job-id and deduplication-owner collisions, removes the old
non-active owner from the jobs hash and state indexes, clears its lock, logs,
dependency key, deduplication owner, and repeat owner only when they still point
at that job, then writes the replacement job, its waiting/delayed index, events,
deduplication key, `repeat:<key>` owner, and scheduler metadata in the same
Redis turn. Lane validates repeat `end_at` before the Redis script is invoked;
if the end timestamp is already earlier than the add/upsert timestamp, the
operation returns a configuration error and leaves job hashes, state indexes,
repeat owners, and scheduler metadata untouched.

This is intentionally a script-level mechanism, not just API-field parity. It is
inspired by BullMQ's use of Lua scripts to maintain repeat scheduler records,
deduplication keys, locks, and state indexes atomically. In BullMQ 5.79.3,
`addJobScheduler-11.lua` stores scheduler metadata in the repeat zset/hash and,
when overriding, removes the previous delayed, prioritized, waiting, or paused
next job before creating the new scheduled job; active/completed/failed
collisions are not blindly overwritten. Lane now keeps the existing
`repeat:<key>` owner key for fast collision checks and also writes a
BullMQ-style scheduler zset at the queue's `repeat` key plus
`repeat_meta:<key>` hashes containing the current owner id, name, next
timestamp, state, count, repeat options, and the schedule-facing fields `key`,
`every`, `pattern`, `limit`, and `endDate` when the Rust repeat options provide
them. Scheduler writes delete and rebuild the metadata hash before `HSET`, so
an overwrite from an interval schedule to a cron schedule cannot leave stale
`every` or `endDate` fields behind. Add, bulk add, flow add, repeat upsert,
repeat successor enqueue, claim-time due promotion, `promote_due_jobs()`,
manual promote, reschedule, active delay/release, retry, remove, clean, drain,
and stalled terminal cleanup update those records inside the same Redis script
that mutates the job state. Non-terminal movement scripts rebuild the
`repeat_meta:<key>` hash from the moved job snapshot, including schedule-facing
fields such as `opts`, `every`, `pattern`, `limit`, and `endDate`; they also
update the scheduler zset score and restore a missing fast `repeat:<key>` owner
key with `SET NX` when the moved job still owns the series. If the fast owner is
missing but scheduler metadata already names a different owner, the movement
script leaves that scheduler record untouched instead of stealing the series.
`get_repeat()`, `count_repeats()`, and `list_repeats_page()` read through the
scheduler zset, validate the owner job snapshot, repair missing fast owner keys
from scheduler metadata, prune stale metadata, and mirror BullMQ's
`getJobScheduler`, `getJobSchedulersCount`, and `getJobSchedulers(start, end,
asc)` read side: entries are ordered by next scheduled time, defaulting to
descending order. Lane still models repeat work as
a Rust-native repeat-series owner and successor enqueue flow rather than a full
BullMQ JS template engine, so exact BullMQ scheduler field-for-field parity
remains a later runtime feature-parity item.

Manual lifecycle management follows the same Redis-side state movement rule:
`promote_job()` removes a delayed job from the delayed zset and inserts it into
waiting inside one script, treats the delayed zset as the Redis movement gate,
rejects retained jobs whose stored state is no longer delayed, and prunes
orphaned or stale delayed members while preserving that state-conflict result.
`reschedule_job()` follows BullMQ's `changeDelay` mechanism: the
script removes the job from the delayed zset, rejects the change if that zset
membership is missing, updates the stored delay and scheduled timestamp, and
adds the job back to the delayed zset with the new score in the same Redis turn.
It also emits BullMQ's `delayed` event with the new delayed timestamp.
`delay_active_job()` follows BullMQ's `moveToDelayed` mechanism for leased
jobs: the script verifies the lock token, treats the active zset as the movement
gate, rejects the move if that active index membership is missing, clears the
lock, updates the stored delay and scheduled timestamp, and writes the delayed
zset member in the same Redis turn. It emits the same delayed timestamp field as
BullMQ's `moveToDelayed` script. `release_active_job()` follows BullMQ's
`moveJobFromActiveToWait` state movement: the script verifies the lock token,
treats the active zset as the movement gate, clears the lock and active lease
fields, resets `processed_at`, and writes the job back into the waiting zset
with its priority score in the same Redis turn. Unlike ordinary adds and retry
requeues, active release writes the job at the start of its priority bucket so
it is claimed before older FIFO or LIFO entries with the same priority, matching
BullMQ's active-to-wait script. When the moved job is the current
repeat-series owner, claim, claim-time delayed promotion, `promote_due_jobs()`,
manual promote, reschedule, active delay, and active release also rebuild the
scheduler hash/zset in the same script and repair a missing fast owner key
instead of leaving the repeat series split across stale Redis keys. They do not
overwrite a scheduler record that already points at another owner.
`retry_job()` follows BullMQ's `reprocessJob` shape for retained failed and
completed jobs: it treats the matching terminal zset as the Redis movement gate,
rejects inconsistent completed/failed index drift after pruning the stale side,
clears terminal metadata (`failed_reason` for failed jobs, `return_value` for
completed jobs, plus processed/finished timestamps), emits `waiting` with
`prev=failed` or `prev=completed`, and moves the job back to waiting inside one
script. For deduplicated failed jobs, that same script reclaims the owner key and
reapplies the deduplication TTL before returning the job to waiting. For
repeat-keyed failed jobs, retry first checks both the fast `repeat:<key>` owner
key and the scheduler `repeat_meta:<key>.jid` owner; if either points at another
non-terminal occurrence, Redis restores the fast owner key when needed and
rejects the retry. Only an uncontested failed owner reclaims the repeat key and
scheduler metadata. When the retried job is a retained flow child, retry restores
the child into the parent's pending dependency set, clears stale deferred parent
failure metadata, and moves a non-terminal parent back to `waiting_children`,
matching BullMQ's dependency restoration path for both failed and completed
children.
When a processing failure reaches terminal failed state because its configured
retry attempts are exhausted, Lane emits `retries-exhausted` after `failed`,
matching BullMQ's `moveToFinished` event order. Manual retry-discard paths only
emit that event if the job had actually reached the configured retry limit.
BullMQ's deprecated `job.discard()` is intentionally modeled as a current
failure-path decision rather than stored job metadata: BullMQ sets an in-memory
`discarded` flag, `shouldRetryJob()` checks that flag before `moveToFailed()`,
and the Redis transition then uses the terminal failed path instead of delayed
or immediate retry. Lane exposes that mechanism as
`fail_job_discarding_retry()` and `JobContext::discard_retry()`. Lane also
mirrors BullMQ's preferred `UnrecoverableError` path with
`LaneError::unrecoverable_job()`: when a processor returns that error, the worker
uses the same retry-bypass finalization path as `discard_retry()`. The Redis
backend reuses the same active-to-failed Lua script as `fail_job()`, but passes
the retry flag as disabled so the script writes the failed zset, releases
deduplication/repeat ownership, and updates flow parents atomically.
`update_priority()`
rewrites the job hash and, for waiting jobs, replaces the waiting zset score in
the same script; for jobs that are no longer waiting, it prunes stale waiting
members while preserving the stored state. Retained terminal jobs can update
their stored priority without being requeued, matching BullMQ's
`changePriority-7.lua` existence-only guard. For waiting jobs, the script also
refreshes `enqueued_seq` and recomputes the FIFO/LIFO score.
`update_priority_with_lifo()` exposes BullMQ's
`changePriority({ priority, lifo })` shape directly: the optional LIFO flag is
stored on `job.options.lifo` before the waiting score is recomputed, so the
Redis index changes together with the serialized job snapshot. This is
intentionally aligned with BullMQ's mechanism of moving job state through Redis
scripts instead of coordinating several client-side Redis commands. Lane also
applies BullMQ's `2^21` priority ceiling before entering that script, so an
invalid update cannot partially rewrite the job hash or waiting index.

Redis job management mutations are script-backed too. `update_data()` follows
BullMQ's `updateData` existence check and write shape, adapted to Lane's Redis
hash layout by decoding the stored job JSON, replacing `payload`, and writing the
job snapshot back in one Lua turn. `update_progress()` mirrors BullMQ's
`updateProgress-3.lua` existence-only guard: any retained job, including a
terminal job, can receive a new progress value, and the script writes that value
plus an `XADD event=progress` entry in one Redis turn. `save_stacktrace()`
mirrors BullMQ's `saveStacktrace` storage behavior:
the Lua script verifies that the retained job exists, decodes Lane's stored job
JSON, replaces the stacktrace array and failure reason together, and writes the
updated snapshot back in one Redis turn. `add_log()` follows BullMQ's `addLog` shape at the key level: the script
verifies that the job exists, `RPUSH`es a structured JSON entry into
`logs:<jobId>`, applies `LTRIM` when a retention count is provided, and mirrors
the retained entries into the job JSON snapshot for Lane compatibility without
emitting a queue event. `clean_jobs()` filters retained records by the parsed
millisecond reference time, and `clean_jobs(JobState::Active, ...)` now mirrors
BullMQ's `clean(..., "active")` guard by cleaning only active jobs whose worker
lock is already gone. Locked active jobs must still finish, fail, be released,
or pass through stalled recovery. Redis clean removes lock keys, hash entries,
state indexes, stalled candidate entries, dependency sets, and log lists
atomically, updates flow parents for removed child jobs, and returns the removed
snapshots. For non-terminal repeat owners, the clean script mirrors BullMQ's
scheduler-job guard: it checks both
`repeat:<key>` and `repeat_meta:<key>.jid`, restores the fast owner key from
valid scheduler metadata, and skips the current series owner instead of deleting
it through broad cleanup.
`RedisJobQueue::remove_orphaned_jobs(count, limit)` is a Redis-only maintenance
helper equivalent to BullMQ's `removeOrphanedJobs()` for Lane's storage layout:
it scans the central jobs hash with `HSCAN`, checks the waiting, delayed, active,
waiting-children, completed, failed, and stalled Redis indexes in Lua, and only
removes a job hash field when none of those keys reference the job id. Removed
orphans also lose their retained `logs:<jobId>` list, `dependencies:<jobId>` set,
and `locks:<jobId>` key in the same Redis turn. Pass `count = 0` to use the
default scan count of 1000, and `limit = 0` to remove all orphans found by the
scan.

Queue draining follows the same rule. `drain_jobs(false)` removes waiting jobs
and `drain_jobs(true)` also removes ordinary delayed jobs in one Redis turn,
while deleting each removed job's retained log list and leaving active,
completed, failed, and waiting-children jobs in place. Like BullMQ's `drain`
script, Lane protects the current delayed repeat
occurrence: BullMQ derives that set from job scheduler records, while Lane
checks the `repeat:<key>` owner key, falls back to `repeat_meta:<key>.jid`, and
restores the fast owner key when scheduler metadata still names the delayed
owner. Removed children update their parent dependency set in the same script,
so a parent can move from `waiting_children` to `waiting`, `delayed`, or `failed`
without a follow-up client pass.

Queue obliteration follows BullMQ's underlying pause-first mechanism rather
than only matching the public method name. BullMQ's public `obliterate()` calls
`pause()` before invoking its Lua command; that command checks `meta.paused`,
rejects active jobs unless `force` is set, and then removes the queue's state,
job, lock, repeat, metrics, and metadata keys. Lane folds the same lifecycle into
one Redis script: it writes `meta.paused`, checks the active sorted-set index,
returns a job-state conflict when active jobs exist and `force` is false, counts
the current job hash, and scans the queue prefix in batches until every matching
key is deleted, including job hashes, lifecycle indexes, locks, retained logs,
deduplication owners, keep-last-if-active shadow jobs, repeat owners, dependency
sets, rate-limit counters, sequence keys, and the pause metadata itself. A failed
non-forced obliteration intentionally leaves `meta.paused` in place, so no worker
can claim additional jobs until the queue is resumed or forcibly obliterated. A
successful forced obliteration removes the pause marker too, leaving an empty,
unpaused queue that can accept fresh jobs with clean deduplication and repeat
ownership.

Queue reads use the same Redis-side snapshot approach. `get_job_state()` follows
BullMQ's `getState` mechanism by checking the Redis state indexes in one script,
rather than trusting the serialized job JSON state field. Lane checks completed,
failed, delayed, active, waiting, and waiting-children sorted sets and returns
`None` when the job id is not present in any state index.
`get_job_finished_result()` follows BullMQ's `isFinished(..., returnValue=true)`
shape: Redis checks the completed and failed indexes plus the retained job hash
in one Lua script, treats those indexes as authoritative even if a retained
snapshot still carries an older state, and returns `NotFinished`, a completed
`return_value`, a failed `failed_reason`, or `None` for missing retained records.
`RedisJobQueue::get_metrics(JobState::Completed | JobState::Failed, start, end)`
follows BullMQ's `getMetrics` storage shape. Complete, fail, and stalled
terminal scripts increment `metrics:<state>` and close one-minute windows into
`metrics:<state>:data` with `LPUSH`/`LTRIM`; reads use the same `HMGET count,
prevTS, prevCount`, `LRANGE`, and `LLEN` script shape. Lane records terminal
metrics by default with `DEFAULT_JOB_METRICS_RETENTION` retained data points.
`get_job_counts()` follows BullMQ's `getCounts` script shape: empty state input
defaults to all lifecycle states, duplicate states are ignored after their first
occurrence, and Redis counts the requested state indexes in one Lua script. Lane
stores every lifecycle state as a sorted set, so the script uses `ZCARD` for
waiting, delayed, active, waiting-children, completed, and failed instead of
loading job snapshots client-side.
`get_job_count()` mirrors BullMQ's `getJobCountByTypes()` getter layer by
summing those per-state counts, so it inherits the same default-all and
duplicate-state semantics. `count_pending_jobs()` mirrors BullMQ's `count()`
meaning: waiting, delayed, and waiting-children jobs are counted as pending
work, while active, completed, and failed jobs are excluded.
`get_counts_per_priority()` follows BullMQ's `getCountsPerPriority` shape for
priority queues: duplicate requested priorities are ignored after their first
occurrence, and Redis counts waiting jobs with `ZCOUNT` over the priority-encoded
waiting zset score range instead of loading job snapshots client-side.
`get_job_logs()` reads the `logs:<jobId>` list with `LRANGE` and `LLEN`,
including BullMQ's descending window convention of using negative indexes and
reversing the result. Missing or already-removed log lists return an empty page.
`clear_job_logs()` follows BullMQ's `Job.clearLogs()` storage behavior: positive
retention uses `LTRIM logs:<jobId> -keep -1`, and zero retention deletes the log
list. Lane also trims the embedded `logs` array in the job snapshot in the same
Redis Lua turn so retained job records and Redis log lists do not drift.
`list_jobs()` follows BullMQ's `getRanges`/`getJobs` mechanism at the Redis
index layer: callers can request one or more lifecycle states and choose
ascending or descending range order. Lane adapts that mechanism to its sorted
state indexes by collecting the selected state members, pruning stale index
entries whose retained job state no longer matches, sorting snapshots by Lane's
stable state/priority/time/id order, and returning the requested page in one Lua
turn.
`stats()` evaluates one Lua script that reads the pause flag and all waiting,
delayed, active, waiting-children, completed, and failed sorted-set counts in a
single Redis turn, mirroring BullMQ's `getCounts` style instead of stitching
together several client-side reads. Redis pause state follows BullMQ's
`meta.paused` mechanism: `pause()` writes the field, `resume()` deletes it, and
`is_paused()` reads that same field. A legacy `paused = 0` value is treated as
resumed and cleaned up.

Stalled recovery is Lua-backed as well. The recovery script follows BullMQ's
`moveStalledJobsToWait` shape: it consumes the previous `stalled` candidate
set, verifies that each candidate's independent lock key is missing, increments
the stalled count, and either requeues the job or fails it in the same Redis
turn. At the end of the script it marks the current active index members in the
`stalled` set for the next recovery pass. Successful `renew_lease()`,
`complete_job()`, `fail_job()`, `delay_active_job()`, and
`release_active_job()` scripts remove the job from the candidate set, mirroring
BullMQ's `extendLock` and `removeLock` helpers. If an active sorted-set member
points at a job that has already moved to a different state, a later recovery
pass prunes that stale active index instead of treating it as recoverable work.
When a candidate is actually recovered, Redis writes a `stalled` event with the
failure reason and then writes the resulting `waiting prev=active` or
`failed prev=active` transition in the same Lua turn, matching the in-memory and
local event contract while preserving BullMQ's explicit `stalled` notification.
BullMQ 5.79.3 also special-cases repeatable scheduler jobs in
`moveStalledJobsToWait-9.lua`: if the scheduler record still exists, the stalled
occurrence is requeued even after the ordinary stalled limit is exceeded. Lane
mirrors that branch for active repeat owners: non-repeat jobs still fail after
`max_stalled_count`, but a stalled repeat owner whose owner key or scheduler
metadata still points at the job is moved back to `waiting` and keeps its
repeat ownership. If the fast `repeat:<key>` owner key is missing but
`repeat_meta:<key>.jid` still names the stalled occurrence, the recovery script
restores the fast owner key before requeueing it.

`remove_job()` uses a Redis script to reject active jobs only while their worker
lock key still exists, matching BullMQ's `removeJob` `isLocked` guard. An active
job whose lock has already disappeared can be removed as stale work; the script
removes the job hash, lock key, all state indexes, stalled candidate entry,
retained log list, and any child dependency set in one Redis turn. A remove
request for a missing job still prunes orphaned indexes, locks, dependency sets,
and log lists for that id. If the removed job is a flow child, the same script
updates the parent's dependency set and atomically moves the parent from
`waiting_children` to `waiting`, `delayed`, or `failed` as appropriate.

Run the Redis integration test against any reachable Redis server:

```bash
A3S_LANE_REDIS_URL=redis://127.0.0.1:6379/ \
  cargo test --features redis-backend --test redis_job_queue
```

The integration harness performs a short TCP reachability preflight before the
test body runs. Missing or unreachable Redis endpoints are reported and skipped
quickly instead of letting every async test wait for its longer per-test timeout.
Namespace cleanup also has bounded Redis command timeouts so a stale test
connection fails clearly instead of hiding the actual failure behind the suite's
outer timeout.

Use `JobWorker` to run async processors against any backend:

```rust
use a3s_lane::{
    job_processor_fn, InMemoryJobQueue, JobOptions, JobProcessor, JobProcessorRouter,
    JobQueueBackend, JobWorker, JobWorkerConfig,
};
use std::{sync::Arc, time::Duration};

# async fn worker_example() -> a3s_lane::Result<()> {
let backend: Arc<dyn JobQueueBackend> = Arc::new(InMemoryJobQueue::new("email"));
backend
    .add_job(
        "send".to_string(),
        serde_json::json!({ "to": "ops@example.com" }),
        JobOptions::new().with_timeout(Duration::from_secs(30)),
    )
    .await?;

let send_processor: Arc<dyn JobProcessor> = Arc::new(job_processor_fn(|job, context| async move {
    context.ensure_lease()?;
    context.update_data(serde_json::json!({ "to": job.payload["to"], "normalized": true })).await?;
    context.update_progress(serde_json::json!({ "phase": "sending" })).await?;
    context.add_log("provider accepted message").await?;
    Ok(serde_json::json!({ "sent": job.payload["to"] }))
}));
let processor = Arc::new(JobProcessorRouter::new().with_processor("send", send_processor));

let worker = JobWorker::new(
    backend,
    processor,
    JobWorkerConfig::new("worker-1")
        .with_concurrency(4)
        .with_blocking_claim_timeout(Duration::from_secs(5)),
);

worker.run_until_idle(100).await?;
# Ok(())
# }
```

Background `JobWorker` loops call `JobQueueBackend::claim_next_blocking()`.
Redis backends use the queue marker zset to sleep until ready or delayed work
wakes them, while in-memory and local durable backends keep the immediate claim
fallback. `run_once()` remains non-blocking for deterministic manual work; use
`run_once_blocking()` when a single worker iteration should wait for new work.
Workers started with `start()` share one lease-renewal loop across concurrent
processors and call `renew_leases()` in batches; direct `run_once()` calls keep
the per-job renewal path so deterministic manual runs do not need a background
worker handle.

`JobContext::has_lost_lease()` and `JobContext::ensure_lease()` let long-running
processors stop before doing more external work after the worker observes a
failed lease renewal. Context progress and log helpers also refuse to write once
that lease-loss flag is set. `JobContext::discard_retry()` lets a processor mark
the current failed finalization as terminal even when the job's retry policy still
has attempts remaining; the marker lives only on the worker context and is not
stored on the job. Returning `LaneError::unrecoverable_job(message)` from a
processor is the preferred typed-error equivalent for failures that should never
be automatically retried.

## Benchmarks

Apple Silicon (M-series), release build, steady-state throughput with pre-warmed manager:

| Workload | Throughput |
|----------|------------|
| 100 commands, 10 lanes | ~33,000–50,000 ops/sec |
| 100 commands, 1 lane | ~6,600–10,000 ops/sec |
| Metrics overhead | ~3–5% |

Full lifecycle benchmarks (including manager create/start/shutdown) run at ~85–93 ops/sec — dominated by startup cost, not scheduling.

```bash
cargo bench
open target/criterion/report/index.html
```

## Community

Join us on [Discord](https://discord.gg/XVg6Hu6H) for questions, discussions, and updates.

## License

MIT
