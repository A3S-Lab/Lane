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
| Generic job runtime | In progress | JSON jobs, bulk submission, idempotent custom job IDs, explicit job states, priority ordering, delayed jobs, token-owned worker leases, completion/failure snapshots, retry backoff, stalled-job recovery, pause/resume. |
| Job management API | In progress | Add/get/remove/promote/retry/update-priority/pause/resume/clean APIs, state queries, pagination, job logs, progress updates, lease renewal. |
| Worker runtime | In progress | `JobWorker` claims jobs from any `JobQueueBackend`, routes jobs by name with `JobProcessorRouter`, runs async processors, completes/fails jobs, supports processor progress/log updates, timeouts, and stalled recovery loops. |
| Durable backend | In progress | `LocalJobQueue` JSON snapshot persistence is available; `RedisJobQueue` is available behind `redis-backend` with Lua-backed claim, complete, fail, renew, and stalled recovery semantics. Postgres/NATS backends remain planned. |
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
lease with the claim token, and `clean_jobs()` removes old records by state.
Set `JobOptions::with_job_id()` when producers need idempotent submission:
adding the same job id again returns the existing job instead of enqueueing a
duplicate.

Every claimed job carries an opaque `lock_token`. Workers must pass that token
to `complete_job()`, `fail_job()`, and `renew_lease()`. This prevents a stale
worker from completing a job after its lease expired and another worker
reclaimed it.

Flow jobs create a parent job and one or more child jobs in a single operation.
The parent starts in `waiting_children`, children are claimed normally, and the
parent is released to `waiting` only after every child completes. A terminal
child failure fails the parent; retryable child failures keep the parent blocked
until the child retries and reaches a terminal outcome.

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
including the first job:

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
# Ok(())
# }
```

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
states with sorted sets, and uses Lua scripts to atomically claim and transition
leased jobs. The Redis backend follows the core BullMQ locking mechanism: a
claim creates an independent TTL lock key for the job, and complete, fail, and
renew operations must prove ownership by matching the lock token before the
script mutates the active/completed/failed/delayed indexes. Stalled recovery
checks the TTL lock key, not only the job JSON snapshot:

```rust
use a3s_lane::{JobOptions, JobQueueBackend, RedisJobQueue, RetryPolicy};
use std::time::Duration;

# async fn redis_example() -> a3s_lane::Result<()> {
let queue = RedisJobQueue::with_namespace(
    "redis://127.0.0.1/",
    "a3s:lane",
    "email",
)?;

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
