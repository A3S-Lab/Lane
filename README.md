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
| Generic job runtime | In progress | JSON jobs, Lua-backed Redis bulk submission, idempotent custom job IDs, simple deduplication with optional TTL, delayed-owner replace, and keep-last-if-active requeue, repeat-key ownership, explicit job states, priority ordering, delayed jobs, token-owned worker leases, completion/failure snapshots, retry backoff, rate-limited claims, shared active concurrency limits, stalled-job recovery, pause/resume. |
| Job management API | In progress | Add/get/remove/promote/retry/update-priority/pause/resume/clean APIs, state queries, pagination, job logs, progress updates, lease renewal. |
| Worker runtime | In progress | `JobWorker` claims jobs from any `JobQueueBackend`, routes jobs by name with `JobProcessorRouter`, runs async processors, completes/fails jobs, supports processor progress/log updates, cooperative lease-loss checks, timeouts, and stalled recovery loops. |
| Durable backend | In progress | `LocalJobQueue` JSON snapshot persistence is available; `RedisJobQueue` is available behind `redis-backend` with Lua-backed add, bulk add, simple deduplication with TTL, delayed-owner replace, keep-last-if-active requeue, repeat-key ownership, flow submission, delayed promotion, single-job promote, manual retry, priority update, progress update, log append, list/stat snapshots, clean, claim, rate limit, max-active, flow parent release/failure, repeat successor enqueue, complete, fail, renew, remove, and stalled recovery semantics. Postgres/NATS backends remain planned. |
| Flow jobs | In progress | Parent-child dependencies, waiting-children state, and fan-out/fan-in release are available across in-memory, local durable, and Redis backends. |
| Repeat jobs | In progress | Fixed-interval and UTC cron repeatable jobs with repeat keys, limits, and end timestamps are available across in-memory, local durable, and Redis backends. |
| SDK and framework parity | Planned | Node/Python typed job APIs, NestJS module, migration guide from BullMQ-compatible concepts. |

The generic job runtime is exposed through the `JobQueueBackend` trait.
`InMemoryJobQueue` is process-local and intended for tests, embedded runtimes,
and reference semantics:

```rust
use a3s_lane::{InMemoryJobQueue, JobOptions, JobQueueBackend, RetryPolicy};
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
paginated `JobListPage` values, `add_jobs()` submits a batch with the same
idempotency semantics as `add_job()`, `promote_job()` moves delayed jobs to
waiting, `retry_job()` manually requeues failed jobs, `update_priority()`
changes non-terminal job priority, `renew_lease()` extends an active worker
lease with the claim token, `remove_job()` removes non-active jobs,
`clean_jobs()` removes old records by state, and both cleanup paths can unblock
flow parents when a pending child is removed.
Set `JobOptions::with_job_id()` when producers need idempotent submission:
adding the same job id again returns the existing job instead of enqueueing a
duplicate.

Every claimed job carries an opaque `lock_token`. Workers must pass that token
to `complete_job()`, `fail_job()`, and `renew_lease()`. This prevents a stale
worker from completing a job after its lease expired and another worker
reclaimed it. Active leased jobs cannot be removed through the normal
management API; run stalled recovery first when a worker lease has expired.

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
                .with_ttl(Duration::from_secs(30)),
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
`replace_delayed(true)` also covers BullMQ's delayed-owner replace path: a new
deduplicated add may remove a delayed standalone owner and insert the new job in
the same operation when the old owner is still present in the delayed index.
For TTL-backed deduplication, replacement preserves the existing owner key's
remaining TTL.
`keep_last_if_active(true)` covers BullMQ's active-owner keep-last path for
standalone jobs: duplicates added while the current owner is active return that
owner, overwrite a queue-local next-job record, and materialize only the latest
duplicate when the owner completes, terminally fails, or exhausts stalled-job
recovery. If that latest duplicate has a delay, the delay starts from the owner
finalization timestamp.
Debounce behavior and flow/repeat keep-last extensions remain planned.
Retrying a failed deduplicated job reclaims the deduplication id while the job is
waiting or active again; retry is rejected if another non-terminal job already
owns that id.

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
states with sorted sets, and uses Lua scripts to atomically add jobs, promote
due delayed jobs, claim work, and transition leased jobs. The Redis backend
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
)?
.with_claim_rate_limit(JobRateLimit::new(100, Duration::from_secs(60)))?;
queue.set_max_active_jobs(32).await?;

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

The claim rate limit is shared through Redis for workers that use the same
namespace and queue. When the window is exhausted, `claim_next()` returns
`None` and the job remains waiting for a later poll.

`set_max_active_jobs()` configures a Redis-shared active job ceiling for the
queue. It stores the value in the queue meta hash as `concurrency`, matching
BullMQ's queue-maxed mechanism. The Lua claim script reads that meta value,
checks the active sorted set count in the same Redis turn, and returns `None`
without moving a job or consuming rate-limit capacity when the queue is already
maxed. `clear_max_active_jobs()` removes the shared ceiling.

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
owner behavior so the key cannot expire while work is still leased.
If `replace_delayed(true)` is set and the current owner is a standalone delayed
job, the add script first removes the old delayed zset member, then removes the
old job hash and inserts the new owner only if that delayed removal succeeded,
mirroring BullMQ's delayed replacement branch. With TTL-backed deduplication, the
script updates the owner id with Redis `KEEPTTL` so replacement does not extend
the remaining deduplication window. If `keep_last_if_active(true)` is set and the
current owner is present in the active sorted set, duplicate adds overwrite a
`deduplication_next:<id>` proto-job record and `PERSIST` the owner key. Complete,
terminal fail, and stalled terminal-fail scripts then atomically delete the old
owner key, materialize that latest proto-job into waiting or delayed state, and
set the deduplication owner to the new job.
Completion, terminal failure, remove, clean, and stalled terminal failure scripts
release deduplication keys only when they still point at the job being finalized
or removed.
Manual retry reclaims the key inside the retry script, reapplies the TTL, and
refuses to move the failed job back to waiting if a newer non-terminal job
already owns the same deduplication id.

Redis flow submission is all-or-nothing: the flow add script first checks every
parent and child job id, then writes the parent, children, and all state indexes
plus the parent's pending dependency set in one Redis turn. If any job id
already exists, no partial parent, child, index, or dependency records are
created.

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
series.

This is intentionally a script-level mechanism, not just API-field parity. It is
inspired by BullMQ's use of Lua scripts to maintain repeat scheduler records,
deduplication keys, locks, and state indexes atomically. A3S Lane's current
repeat support is still a lightweight repeat-series owner and successor enqueue
model; full BullMQ scheduler management APIs remain a later SDK/runtime parity
item.

Manual lifecycle management follows the same Redis-side state movement rule:
`promote_job()` removes a delayed job from the delayed zset and inserts it into
waiting inside one script, treats the delayed zset as the Redis movement gate,
and prunes orphaned or stale delayed members when a job is missing or already in
another state. `retry_job()` clears terminal failure metadata, treats the failed
zset as the Redis movement gate, prunes orphaned or stale failed members, and
moves valid failed jobs back to waiting inside one script. For deduplicated and
repeat-keyed jobs, that same script reclaims the owner key before returning the
job to waiting; deduplication TTL is re-applied during that same retry script.
`update_priority()`
rewrites the job hash and, for waiting jobs, replaces the waiting zset score in
the same script; for jobs that are no longer waiting, it prunes stale waiting
members while preserving the stored non-terminal state. This is intentionally
aligned with BullMQ's mechanism of moving job state through Redis scripts instead
of coordinating several client-side Redis commands.

Redis job management mutations are script-backed too. `update_progress()` checks
the current state and writes the progress value in one Redis turn; `add_log()`
appends and trims retained log entries inside one script; `clean_jobs()` filters
retained records by the parsed millisecond reference time, removes their lock
keys, hash entries, and state indexes atomically, updates flow parents for
removed child jobs, and returns the removed snapshots.

Queue reads use the same Redis-side snapshot approach. `list_jobs()` evaluates
one Lua script to read state pages and job JSON snapshots in the same Redis turn
and to prune stale state-index entries it encounters. `stats()` evaluates one
Lua script that reads the pause flag and all waiting, delayed, active,
waiting-children, completed, and failed sorted-set counts in a single Redis
turn, mirroring BullMQ's `getCounts` style instead of stitching together
several client-side reads.

Stalled recovery is Lua-backed as well. The recovery script scans expired
active scores, verifies that the independent lock key is missing, increments
the stalled count, and either requeues the job or fails it in the same Redis
turn. If an active sorted-set member points at a job that has already moved to a
different state, the same script prunes that stale active index instead of
treating it as recoverable work.

`remove_job()` uses a Redis script to reject active jobs and remove the job
hash, lock key, all state indexes, and any child dependency set in one Redis
turn. A remove request for a missing job still prunes orphaned indexes, locks,
and dependency sets for that id. If the removed job is a flow child, the same
script updates the parent's dependency set and atomically moves the parent from
`waiting_children` to `waiting`, `delayed`, or `failed` as appropriate.

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
that lease-loss flag is set.

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
