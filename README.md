# a3s-lane

Lane-based priority queue for concurrent async tasks. Commands are organized into named lanes with configurable concurrency and priority — the highest-priority lane with pending work is always scheduled next.

Used in the A3S ecosystem to guarantee control commands (pause/cancel) always preempt LLM generation: `control` (P=1) beats `prompt` (P=5) regardless of arrival order.

[![crates.io](https://img.shields.io/crates/v/a3s-lane.svg)](https://crates.io/crates/a3s-lane)
[![PyPI](https://img.shields.io/pypi/v/a3s-lane.svg)](https://pypi.org/project/a3s-lane/)
[![npm](https://img.shields.io/npm/v/@a3s-lab/lane.svg)](https://www.npmjs.com/package/@a3s-lab/lane)

## Install

```toml
[dependencies]
a3s-lane = "0.4"
```

All four features (`distributed`, `metrics`, `monitoring`, `telemetry`) are on by default. Core queue only:

```toml
a3s-lane = { version = "0.4", default-features = false }
# or pick selectively:
a3s-lane = { version = "0.4", default-features = false, features = ["metrics", "distributed"] }
```

Enable the optional Redis generic job backend for multi-process workers:

```toml
a3s-lane = { version = "0.4", features = ["redis-backend"] }
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

## SDKs

```bash
pip install a3s-lane        # Python (PyO3/maturin)
npm install @a3s-lab/lane   # Node.js (napi-rs)
```

Both SDKs expose the full v0.4 API: default lanes, custom lanes, submit, subscribe, drain.

### Python

```python
from a3s_lane import Lane, LaneConfig

# Default lanes
lane = Lane()
lane.start()

# Custom lanes
lane = Lane.with_lanes([
    LaneConfig("high", priority=0, min_concurrency=1, max_concurrency=4),
    LaneConfig("low",  priority=1, min_concurrency=1, max_concurrency=2),
])
lane.start()

# Submit — blocks until the command completes
result = lane.submit("high", "my_command", {"key": "value"})

# Subscribe — blocks until the next event (optional timeout)
stream = lane.subscribe()
event = stream.recv(timeout_ms=5000)   # returns None on timeout
if event:
    print(event.key, event.payload)

# Filtered subscription — exact key match
failures = lane.subscribe_filtered([
    "queue.command.failed",
    "queue.command.timeout",
])

# Graceful shutdown
lane.shutdown()
lane.drain(timeout_secs=30.0)
```

### Node.js

```js
const { Lane } = require('@a3s-lab/lane');

// Default lanes
const lane = new Lane();
lane.start();

// Custom lanes
const lane = Lane.withLanes([
  { laneId: 'high', priority: 0, minConcurrency: 1, maxConcurrency: 4 },
  { laneId: 'low',  priority: 1, minConcurrency: 1, maxConcurrency: 2 },
]);
lane.start();

// Submit — returns JSON string
const result = JSON.parse(lane.submit('high', 'my_command', JSON.stringify({ key: 'value' })));

// Subscribe — callback receives (err, event) for every event
lane.subscribe((err, event) => {
  if (err) throw err;
  console.log(event.key, JSON.parse(event.payload));
});

// Filtered subscription — exact key match
lane.subscribeFiltered(
  ['queue.command.failed', 'queue.command.timeout'],
  (err, event) => { console.error('failure:', event.key); }
);

// Graceful shutdown
lane.shutdown();
lane.drain(30_000);  // timeout in ms
```

## Development

```bash
just test       # 246 tests, --all-features
just ci         # fmt + clippy + test
just bench      # Criterion benchmarks → target/criterion/report/index.html
just cov        # coverage report (requires cargo-llvm-cov)
just doc        # generate and open rustdoc
```

Optional: `cargo install cargo-llvm-cov`, `brew install lcov` (HTML coverage).

## In the A3S ecosystem

a3s-lane is the scheduling layer of the A3S Agent OS. Each a3s-code agent session gets its own instance, ensuring control commands always preempt LLM work:

```
a3s-gateway → a3s-box (MicroVM) → SafeClaw → a3s-code → a3s-lane
                                                          ↑ here
```

Works standalone for any priority-based async scheduling: web servers, background job processors, rate-limited API clients.

## Universal job queue roadmap

A3S Lane is evolving from an in-process lane scheduler into a general
distributed priority job queue. The direction is BullMQ-like, but native to the
A3S stack and language SDKs.

| Phase | Status | Scope |
| --- | --- | --- |
| Lane scheduler | Done | Lane priorities, per-lane concurrency, command retries, timeout, DLQ, events, metrics, monitoring. |
| Generic job runtime | In progress | JSON jobs, Lua-backed Redis bulk submission, idempotent custom job IDs, simple deduplication with optional TTL, debounce TTL extension, delayed-owner replace, and keep-last-if-active requeue, repeat-key ownership, explicit job states, priority ordering, delayed jobs, token-owned worker leases, active-to-wait/delayed movement, completion/failure snapshots, retry backoff, Redis-shared rate-limit and active-concurrency controls, stalled-job recovery, pause/resume. |
| Job management API | In progress | Add/get/get-state/get-job-counts/get-job-count/count-pending/remove/remove-repeat/remove-deduplication-key/get-deduplication-job-id/list-repeats/get-flow-dependencies/get-flow-dependency-counts/remove-unprocessed-children/remove-child-dependency/promote/reschedule/delay-active/release-active/retry/update-priority/update-data/pause/resume/is-paused/drain/clean/obliterate APIs, multi-state pagination, ascending/descending listing, waiting priority counts, add-log/get-logs/clear-job-logs, progress updates, lease renewal. |
| Worker runtime | In progress | `JobWorker` claims jobs from any `JobQueueBackend`, routes jobs by name with `JobProcessorRouter`, runs async processors, completes/fails jobs, supports processor progress/log updates, cooperative lease-loss checks, timeouts, and stalled recovery loops. |
| Durable backend | In progress | `LocalJobQueue` JSON snapshot persistence is available; `RedisJobQueue` is available behind `redis-backend` with Lua-backed add, bulk add, simple deduplication with TTL, debounce TTL extension, delayed-owner replace, keep-last-if-active requeue, deduplication-key removal, repeat-key ownership/listing/removal, flow submission, flow dependency inspection, delayed promotion and rescheduling, active-to-wait/delayed movement, single-job promote, state-index queries, job count snapshots, manual retry, priority update, progress update, log append, list/stat snapshots, drain, clean, obliterate, claim, Redis-shared rate limit, max-active, flow parent release/failure, repeat successor enqueue, complete, fail, renew, remove, and stalled recovery semantics. Postgres/NATS backends remain planned. |
| Flow jobs | In progress | Parent-child dependencies, waiting-children state, dependency inspection, and fan-out/fan-in release are available across in-memory, local durable, and Redis backends. |
| Repeat jobs | In progress | Fixed-interval and UTC cron repeatable jobs with repeat keys, limits, end timestamps, and repeat-key removal are available across in-memory, local durable, and Redis backends. |
| SDK and framework parity | Planned | Node/Python typed job APIs, NestJS module, migration guide from BullMQ-compatible concepts. |

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
manually requeues failed jobs, `fail_job_discarding_retry()` fails an active
token-owned job without applying remaining automatic retries, `update_priority()`
changes non-terminal job priority, `renew_lease()` extends an active worker
lease with the claim token,
`remove_job()` removes non-active jobs,
`remove_repeat()` removes the current non-active owner for a repeat key,
`remove_deduplication_key()` clears the active owner for a deduplication id,
`get_deduplication_job_id()` returns the current owner job id for a
deduplication id, `list_repeats()` lists current non-terminal repeat-series
owners,
`get_flow_dependencies()` returns a flow parent's child snapshots plus pending
and missing child ids, `get_flow_dependency_counts()` returns processed,
unprocessed, failed, and missing child counts, `remove_unprocessed_children()`
removes children that are still unprocessed and not active,
`remove_child_dependency()` detaches one unfinished child from its parent without
deleting the child job,
`drain_jobs(false)` removes waiting jobs, `drain_jobs(true)` also removes
ordinary delayed jobs while preserving current delayed repeat owners,
`clean_jobs()` removes old records by state, `obliterate(false)` pauses the
queue and removes all queue data only when no active jobs exist,
`obliterate(true)` forces removal even with active jobs, `get_job_counts()`
returns per-state counts, `get_job_count()` returns aggregate counts for
selected states, `count_pending_jobs()` returns waiting, delayed, and
waiting-children work, `get_counts_per_priority()` returns waiting-job counts
for selected priorities, `update_data()` replaces a retained job payload,
`add_log()` appends retained job logs, and
`get_job_logs()` returns a `JobLogPage` with Redis/BullMQ-style range semantics.
`clear_job_logs(job_id, 0)` clears retained logs for a job, while positive
values keep the newest entries.
`pause()`, `resume()`, and `is_paused()` provide queue-level dispatch control.
Cleanup paths can unblock flow parents when a pending child is removed.
Set `JobOptions::with_job_id()` when producers need idempotent submission:
adding the same job id again returns the existing job instead of enqueueing a
duplicate.

Every claimed job carries an opaque `lock_token`. Workers must pass that token
to `complete_job()`, `fail_job()`, `fail_job_discarding_retry()`, and
`renew_lease()`. This prevents a stale worker from completing or failing a job
after its lease expired and another worker reclaimed it. Active leased jobs
cannot be removed through the normal management API; run stalled recovery first
when a worker lease has expired.

Flow jobs create a parent job and one or more child jobs in a single operation.
The parent starts in `waiting_children`, children are claimed normally, and the
parent is released to `waiting` after every remaining child completes or is
removed. A terminal child failure fails the parent; retryable child failures
keep the parent blocked until the child retries and reaches a terminal outcome.

```rust
use a3s_lane::{InMemoryJobQueue, JobOptions, JobSpec, JobState};

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
that owner instead of creating a parallel repeat chain:

```rust
use a3s_lane::{InMemoryJobQueue, JobOptions, RepeatOptions};
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

let repeats = queue.list_repeats().await?;
assert_eq!(repeats[0].key, "crm-heartbeat");
assert_eq!(repeats[0].job_id, job.id);

let removed = queue.remove_repeat("crm-heartbeat").await?;
assert_eq!(removed.as_ref().map(|job| job.id.as_str()), Some(job.id.as_str()));
# Ok(())
# }
```

Simple deduplication coalesces duplicate submissions while the first matching
job is still non-terminal. An optional TTL limits how long a non-terminal job
owns its deduplication id:

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

The current deduplication mode intentionally covers BullMQ's simple mode: a
deduplication id blocks duplicate adds until the owning job completes, fails
terminally, is removed, is cleaned, or its configured TTL expires.
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
regular successor for that finalization turn. Flow keep-last extensions remain
planned.
Retrying a failed deduplicated job reclaims the deduplication id while the job is
waiting or active again; retry is rejected if another non-terminal job already
owns that id.
`remove_deduplication_key()` clears the queue's current owner for a
deduplication id before that owner reaches a terminal state, matching BullMQ's
queue-level `removeDeduplicationKey()` behavior of deleting the Redis
deduplication key. The original job remains in its current state, but later
submissions with the same deduplication id can become the new owner.
`get_deduplication_job_id()` mirrors BullMQ's `getDeduplicationJobId()` getter
by returning the current owner job id for that deduplication id.

Use `LocalJobQueue` when a process-local runtime needs durable restart
recovery:

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
and transition leased jobs. The Redis backend
follows the core BullMQ locking mechanism: a claim creates an independent TTL
lock key for the job, and complete, fail, and renew operations must prove
ownership by matching the lock token before the script mutates the
active/completed/failed/delayed indexes. Stalled recovery checks the TTL lock
key, not only the job JSON snapshot:

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
    .claim_next("worker-1".to_string(), Duration::from_secs(30), chrono::Utc::now())
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
poll.

`set_max_active_jobs()` configures a Redis-shared active job ceiling for the
queue. It stores the value in the queue meta hash as `concurrency`, matching
BullMQ's queue-maxed mechanism. `get_max_active_jobs()` reads that same meta
field, mirroring BullMQ's global concurrency getter. The Lua claim script reads
the meta value, checks the active sorted set count in the same Redis turn, and
returns `None` without moving a job or consuming rate-limit capacity when the
queue is already maxed. `clear_max_active_jobs()` removes the shared ceiling.

Like BullMQ's `moveToActive` script, Redis claims also promote due delayed jobs
inside the same Lua script before checking pause, rate-limit, max-active, and
the next claim. A paused or maxed queue can still move due delayed jobs back to
`waiting`; it simply returns `None` instead of leasing work. Claiming also
validates the stored job state before moving a waiting-index entry to `active`,
pruning stale waiting sorted-set entries instead of reactivating jobs that have
already moved elsewhere.

Redis adds are Lua-backed as well. The add scripts write job JSON and the
waiting, delayed, or waiting-children index in the same Redis turn. If a custom
job id already exists, the script returns the existing job without advancing the
waiting sequence or writing duplicate state indexes. Bulk add follows the same
mechanism in one script call while preserving the caller's input order.
For simple deduplication, the same add scripts use an independent
`deduplication:<id>` key, equivalent to BullMQ's `de:<id>` role, to return the
currently active job before writing a duplicate. If `DeduplicationOptions` has a
TTL, the Lua scripts write that owner key with `PX` so Redis expires the
deduplication window even while the original job remains non-terminal. The
keep-last-if-active mode intentionally omits that TTL, matching BullMQ's active
owner behavior so the key cannot expire while work is still leased. If
`extend_ttl(true)` is set, duplicate adds refresh the owner key with `PX` before
returning the current owner, matching BullMQ's debounce extension branch.
If `replace_delayed(true)` is set and the current owner is a standalone delayed
job, the add script first removes the old delayed zset member, then removes the
old job hash and inserts the new owner only if that delayed removal succeeded,
mirroring BullMQ's delayed replacement branch. With TTL-backed deduplication, the
script updates the owner id with Redis `KEEPTTL` so replacement does not extend
the remaining deduplication window unless `extend_ttl(true)` is also set. If
`keep_last_if_active(true)` is set and the current owner is present in the
active sorted set, duplicate adds overwrite a
`deduplication_next:<id>` proto-job record and `PERSIST` the owner key. Complete,
terminal fail, and stalled terminal-fail scripts then atomically delete the old
owner key, materialize that latest proto-job into waiting or delayed state, and
set the deduplication owner to the new job. When the owner and latest duplicate
share the same repeat key, the finalization script also increments
`repeat_count`, sets the `repeat:<key>` owner to the materialized latest job, and
suppresses the regular repeat successor for that turn. This preserves the
single-owner repeat invariant while matching BullMQ's keep-last requeue
mechanism, where the dedup-next record is consumed during job finalization rather
than by a later client-side pass.
Completion, terminal failure, remove, clean, and stalled terminal failure scripts
release deduplication keys only when they still point at the job being finalized
or removed.
Manual retry reclaims the key inside the retry script, reapplies the TTL, and
refuses to move the failed job back to waiting if a newer non-terminal job
already owns the same deduplication id.
`remove_deduplication_key()` deletes `deduplication:<id>` directly, so a later
add can claim the same id even while the old owner remains non-terminal. The
in-memory and local durable backends persist the same logical release by
tracking the released owner id in their snapshots instead of relying on a
client-side scan alone. `get_deduplication_job_id()` reads that same
`deduplication:<id>` key, matching BullMQ's `GET de:<id>` getter path. If the
key points at a missing or mismatched job, Redis cleans up the stale key and
reports no owner.

Redis flow submission is all-or-nothing: the flow add script first checks every
parent and child job id, then writes the parent, children, and all state indexes
plus the parent's pending dependency set in one Redis turn. If any job id
already exists, no partial parent, child, index, or dependency records are
created. `get_flow_dependencies()` uses a Redis-side read script to load the
parent and every retained child snapshot from the jobs hash in one turn, and
returns the child ids that are still pending or missing from retention.
`get_flow_dependency_counts()` follows BullMQ's `getDependencyCounts` Redis/Lua
mechanism instead of only copying the API names. BullMQ 5.79.2 counts
parent-scoped `:processed`, `:dependencies`, `:failed`, and `:unsuccessful`
structures with `HLEN`, `SCARD`, `HLEN`, and `ZCARD`. Lane keeps child snapshots
in the queue jobs hash and keeps the still-blocking children in
`dependencies:<parent_id>`, so the Redis count script reads both structures in
one turn and returns processed, unprocessed, failed, and missing totals without
returning every child snapshot to the client. Lane does not currently expose
BullMQ's ignored-child bucket.
`remove_unprocessed_children()` follows BullMQ's `removeUnprocessedChildren`
script shape at the dependency-set level: it removes children that are still in
the parent's pending dependency set, skips completed, failed, active, or locked
children, deletes the removed child records and per-child metadata in the same
Redis turn, then checks whether the parent can leave `waiting_children`. Lane
returns the removed child snapshots for auditability while preserving the parent
`child_ids`, so later dependency inspection reports removed children as missing.
`remove_child_dependency()` follows BullMQ's `removeChildDependency` path: it
removes one child from the parent's pending dependency set, clears the child's
parent reference, keeps the child job itself, and releases the parent when no
pending dependencies remain. Because Lane stores parent child references in the
parent snapshot, it also removes that child id from `child_ids` so later
dependency reads reflect the broken relationship instead of treating the child as
missing.

Flow fan-in is also protected in Redis transitions. Redis flow submission writes
a pending dependency set for the parent, and child completion, removal, and
cleanup scripts remove the child id from that set before checking whether the
parent can be released to `waiting`, parked in `delayed` until its own schedule
is due, or failed because a child reached terminal failure. This follows
BullMQ's dependency-removal mechanism: cleanup that removes a child also updates
the parent dependency state instead of relying on a later client-side cleanup
pass.

Repeat successors are created during the Redis completion script too. The
worker computes the next occurrence from `RepeatOptions`, then the Lua script
finishes the current job and writes the next delayed or waiting occurrence in
the same Redis turn. Redis also keeps a lightweight `repeat:<key>` owner key for
each active repeat series. The add scripts check that key before inserting a new
repeat job, the completion script transfers ownership to the successor before
releasing the completed occurrence, and terminal failure, remove, clean, and
stalled terminal failure release the key only if it still points at the job being
finalized or removed. Manual retry reclaims the repeat key inside the retry
script and rejects retry if another non-terminal occurrence already owns the
series. `list_repeats()` scans the queue's `repeat:<key>` owner keys, loads
each owner job snapshot from the jobs hash, returns only non-terminal matching
owners, and clears stale owner keys that point at missing, terminal, or
mismatched jobs. `remove_repeat()` resolves the current `repeat:<key>` owner and
then runs the same Redis-side removal path as `remove_job()`, so it rejects
active leased owners, removes the job hash and state indexes, releases repeat
and deduplication ownership, and can unblock flow parents. If the owner key
points at a missing job, Redis clears that stale owner key only when it still
points at the missing id.

This is intentionally a script-level mechanism, not just API-field parity. It is
inspired by BullMQ's use of Lua scripts to maintain repeat scheduler records,
deduplication keys, locks, and state indexes atomically, including BullMQ's
`removeJobScheduler` and legacy `removeRepeatable` scripts that remove both the
repeat scheduler metadata and the current delayed occurrence. A3S Lane's current
repeat support is still a lightweight repeat-series owner and successor enqueue
model; full BullMQ scheduler upsert APIs, pagination, and richer scheduler
metadata remain later SDK/runtime parity items.

Manual lifecycle management follows the same Redis-side state movement rule:
`promote_job()` removes a delayed job from the delayed zset and inserts it into
waiting inside one script, treats the delayed zset as the Redis movement gate,
and prunes orphaned or stale delayed members when a job is missing or already in
another state. `reschedule_job()` follows BullMQ's `changeDelay` mechanism: the
script removes the job from the delayed zset, rejects the change if that zset
membership is missing, updates the stored delay and scheduled timestamp, and
adds the job back to the delayed zset with the new score in the same Redis turn.
`delay_active_job()` follows BullMQ's `moveToDelayed` mechanism for leased
jobs: the script verifies the lock token, treats the active zset as the movement
gate, rejects the move if that active index membership is missing, clears the
lock, updates the stored delay and scheduled timestamp, and writes the delayed
zset member in the same Redis turn. `release_active_job()` follows BullMQ's
`moveJobFromActiveToWait` state movement: the script verifies the lock token,
treats the active zset as the movement gate, clears the lock and active lease
fields, resets `processed_at`, and writes the job back into the waiting zset
with its priority score in the same Redis turn.
`retry_job()` clears terminal failure metadata, treats the failed zset as the
Redis movement gate, prunes orphaned or stale failed members, and moves valid
failed jobs back to waiting inside one script. For deduplicated and repeat-keyed
jobs, that same script reclaims the owner key before returning the job to
waiting; deduplication TTL is re-applied during that same retry script.
BullMQ's deprecated `job.discard()` is intentionally modeled as a current
failure-path decision rather than stored job metadata: BullMQ sets an in-memory
`discarded` flag, `shouldRetryJob()` checks that flag before `moveToFailed()`,
and the Redis transition then uses the terminal failed path instead of delayed
or immediate retry. Lane exposes that mechanism as
`fail_job_discarding_retry()` and `JobContext::discard_retry()`. The Redis
backend reuses the same active-to-failed Lua script as `fail_job()`, but passes
the retry flag as disabled so the script writes the failed zset, releases
deduplication/repeat ownership, and updates flow parents atomically.
`update_priority()`
rewrites the job hash and, for waiting jobs, replaces the waiting zset score in
the same script; for jobs that are no longer waiting, it prunes stale waiting
members while preserving the stored non-terminal state. This is intentionally
aligned with BullMQ's mechanism of moving job state through Redis scripts instead
of coordinating several client-side Redis commands.

Redis job management mutations are script-backed too. `update_data()` follows
BullMQ's `updateData` existence check and write shape, adapted to Lane's Redis
hash layout by decoding the stored job JSON, replacing `payload`, and writing the
job snapshot back in one Lua turn. `update_progress()` checks the current state
and writes the progress value in one Redis turn. `add_log()` follows BullMQ's
`addLog` shape at the key level: the script verifies that the job exists,
`RPUSH`es a structured JSON entry into `logs:<jobId>`, applies `LTRIM` when a
retention count is provided, and mirrors the retained entries into the job JSON
snapshot for Lane compatibility. `clean_jobs()` filters retained records by the
parsed millisecond reference time, removes their lock keys, hash entries, state
indexes, dependency sets, and log lists atomically, updates flow parents for
removed child jobs, and returns the removed snapshots.

Queue draining follows the same rule. `drain_jobs(false)` removes waiting jobs
and `drain_jobs(true)` also removes ordinary delayed jobs in one Redis turn,
while deleting each removed job's retained log list and leaving active,
completed, failed, and waiting-children jobs in place. Like BullMQ's `drain`
script, Lane protects the current delayed repeat
occurrence: BullMQ derives that set from job scheduler records, while Lane
checks the `repeat:<key>` owner key and skips the delayed job when it is still
the current series owner. Removed children update their parent dependency set in
the same script, so a parent can move from `waiting_children` to `waiting`,
`delayed`, or `failed` without a follow-up client pass.

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

Stalled recovery is Lua-backed as well. The recovery script scans expired
active scores, verifies that the independent lock key is missing, increments
the stalled count, and either requeues the job or fails it in the same Redis
turn. If an active sorted-set member points at a job that has already moved to a
different state, the same script prunes that stale active index instead of
treating it as recoverable work.

`remove_job()` uses a Redis script to reject active jobs and remove the job
hash, lock key, all state indexes, retained log list, and any child dependency
set in one Redis turn. A remove request for a missing job still prunes orphaned
indexes, locks, dependency sets, and log lists for that id. If the removed job is
a flow child, the same script updates the parent's dependency set and atomically
moves the parent from `waiting_children` to `waiting`, `delayed`, or `failed` as
appropriate.

Run the Redis integration test against any reachable Redis server:

```bash
A3S_LANE_REDIS_URL=redis://127.0.0.1:6379/ \
  cargo test --features redis-backend --test redis_job_queue
```

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
    JobWorkerConfig::new("worker-1").with_concurrency(4),
);

worker.run_until_idle(100).await?;
# Ok(())
# }
```

`JobContext::has_lost_lease()` and `JobContext::ensure_lease()` let long-running
processors stop before doing more external work after the worker observes a
failed lease renewal. Context progress and log helpers also refuse to write once
that lease-loss flag is set. `JobContext::discard_retry()` lets a processor mark
the current failed finalization as terminal even when the job's retry policy still
has attempts remaining; the marker lives only on the worker context and is not
stored on the job.

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
