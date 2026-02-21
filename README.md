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
