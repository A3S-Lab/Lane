# A3S Lane

<p align="center">
  <strong>Per-Session Priority Queue</strong>
</p>

<p align="center">
  <em>Scheduling layer — each a3s-code agent session gets its own a3s-lane instance for priority-based command scheduling</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#sdk">SDK</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#api-reference">API Reference</a> •
  <a href="#development">Development</a>
</p>

<p align="center">
  <a href="https://crates.io/crates/a3s-lane"><img src="https://img.shields.io/crates/v/a3s-lane.svg" alt="crates.io"></a>
  <a href="https://pypi.org/project/a3s-lane/"><img src="https://img.shields.io/pypi/v/a3s-lane.svg" alt="PyPI"></a>
  <a href="https://www.npmjs.com/package/@a3s-lab/lane"><img src="https://img.shields.io/npm/v/@a3s-lab/lane.svg" alt="npm"></a>
</p>

---

## Overview

**A3S Lane** provides a lane-based priority command queue designed for managing concurrent async operations with different priority levels. In the A3S ecosystem, each a3s-code agent session gets its own a3s-lane instance — ensuring control commands (pause/cancel) always preempt LLM generation tasks. Commands are organized into lanes, each with configurable concurrency limits and priority.

### Basic Usage

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, Command, Result};
use async_trait::async_trait;

// Define a command
struct MyCommand {
    data: String,
}

#[async_trait]
impl Command for MyCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"processed": self.data}))
    }

    fn command_type(&self) -> &str {
        "my_command"
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create event emitter
    let emitter = EventEmitter::new(100);

    // Build queue manager with default lanes
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build()
        .await?;

    // Start the scheduler
    manager.start().await?;

    // Submit a command
    let cmd = Box::new(MyCommand { data: "hello".to_string() });
    let rx = manager.submit("query", cmd).await?;

    // Wait for result
    let result = rx.await??;
    println!("Result: {}", result);

    Ok(())
}
```

## Features

### Core (always compiled)
- **Priority-based Scheduling**: Commands execute based on lane priority (lower = higher priority)
- **Concurrency Control**: Per-lane min/max concurrency limits
- **6 Built-in Lanes**: system, control, query, session, skill, prompt
- **Command Timeout**: Configurable timeout per lane with automatic cancellation
- **Retry Policies**: Exponential backoff, fixed delay, or no-retry strategies
- **Dead Letter Queue**: Capture permanently failed commands for inspection
- **Persistent Storage**: Optional pluggable storage backend (`LocalStorage` included)
- **Graceful Shutdown**: Drain pending commands before shutdown
- **Event System**: 11 lifecycle events emitted automatically (submit, start, complete, retry, DLQ, fail, timeout, shutdown)
- **Builder Pattern**: Flexible queue configuration via `QueueManagerBuilder`
- **Async-first**: Built on Tokio for high-performance async operations

### `distributed` feature (default)
- **Rate Limiting**: Token bucket and sliding window rate limiters, enforced per-lane at dequeue time
- **Priority Boosting**: Deadline-based priority boost applied to the front command on each scheduling tick
- **Queue Partitioning**: Round-robin, hash-based, and custom partitioning strategies
- **Multi-core Parallelism**: Automatic CPU core detection via `PartitionConfig::auto()`
- **Distributed Queue**: Pluggable `DistributedQueue` trait for multi-machine processing

### `metrics` feature (default)
- **Metrics Collection**: Local in-memory metrics with pluggable `MetricsBackend` trait
- **Latency Histograms**: Command execution and wait time with percentiles (p50, p90, p95, p99)

### `monitoring` feature (default, requires `metrics`)
- **Queue Depth Alerts**: Configurable warning and critical thresholds
- **Latency Alerts**: Monitor and alert on command execution latency
- **Health Monitoring**: Background `QueueMonitor` tracking depth and active counts

### `telemetry` feature (default, requires `metrics`)
- **OpenTelemetry Integration**: Native OTLP metrics export via `OtelMetricsBackend`

### SDKs
- **Python SDK**: `pip install a3s-lane` — async queue management from Python
- **Node.js SDK**: `npm install @a3s-lab/lane` — native bindings for Node.js

## Quality Metrics

### Test Coverage

**236 unit tests** across all modules (`--all-features`). Run the live coverage report:

```bash
cargo llvm-cov --lib --summary-only --all-features
```

### Performance Benchmarks

Criterion-based benchmarks measure throughput, concurrency scaling, and overhead:

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench --bench queue_benchmark
```

**Benchmark suites:**
- **Throughput**: Measures commands/second for 1K, 10K, 50K, and 100K command batches
- **Concurrency scaling**: Tests performance with 1, 2, 4, 8, and 16 concurrent lanes
- **Priority scheduling**: Compares overhead of priority-based vs FIFO scheduling
- **Metrics overhead**: Measures performance impact of metrics collection

Results are saved to `target/criterion/` with detailed HTML reports.

## Architecture

### Lane Priority Model

| Lane | Priority | Default Max Concurrency | Use Case |
|------|----------|------------------------|----------|
| `system` | 0 (highest) | 5 | System-level operations |
| `control` | 1 | 3 | Control commands (pause, resume, cancel) |
| `query` | 2 | 10 | Read-only queries |
| `session` | 3 | 5 | Session management |
| `skill` | 4 | 3 | Skill/tool execution |
| `prompt` | 5 (lowest) | 2 | LLM prompt processing |

### Components

```
┌─────────────────────────────────────────┐
│            QueueManager                 │
│  ┌─────────────────────────────────┐   │
│  │         CommandQueue            │   │
│  │  ┌───────┐ ┌───────┐ ┌───────┐ │   │
│  │  │system │ │control│ │ query │ │   │
│  │  │ P=0   │ │ P=1   │ │ P=2   │ │   │
│  │  └───────┘ └───────┘ └───────┘ │   │
│  │  ┌───────┐ ┌───────┐ ┌───────┐ │   │
│  │  │session│ │ skill │ │prompt │ │   │
│  │  │ P=3   │ │ P=4   │ │ P=5   │ │   │
│  │  └───────┘ └───────┘ └───────┘ │   │
│  └─────────────────────────────────┘   │
└─────────────────────────────────────────┘
            ↓ schedule_next()
    Priority-based command selection
```

## Quick Start

### Installation

```toml
[dependencies]
a3s-lane = "0.3"
```

All four features (`metrics`, `monitoring`, `telemetry`, `distributed`) are on by default. To use only the core queue without optional deps:

```toml
[dependencies]
a3s-lane = { version = "0.3", default-features = false }
```

Enable individual features selectively:

```toml
[dependencies]
a3s-lane = { version = "0.3", default-features = false, features = ["metrics", "distributed"] }
```

### Custom Lanes

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, LaneConfig};

let emitter = EventEmitter::new(100);
let manager = QueueManagerBuilder::new(emitter)
    .with_lane("high-priority", LaneConfig::new(1, 4), 0)
    .with_lane("normal", LaneConfig::new(1, 8), 1)
    .with_lane("background", LaneConfig::new(1, 2), 2)
    .build()
    .await?;
```

### Queue Monitoring

```rust
use a3s_lane::{QueueMonitor, MonitorConfig};
use std::time::Duration;
use std::sync::Arc;

let config = MonitorConfig {
    interval: Duration::from_secs(5),
    pending_warning_threshold: 50,
    active_warning_threshold: 25,
};

let monitor = Arc::new(QueueMonitor::with_config(
    manager.queue(),
    config,
));

// Start monitoring (runs in background)
monitor.clone().start().await;

// Get current stats
let stats = monitor.stats().await;
println!("Pending: {}, Active: {}", stats.total_pending, stats.total_active);
```

### Event Subscription

The queue emits lifecycle events automatically. Subscribe before starting the scheduler:

```rust
use a3s_lane::EventEmitter;

let emitter = EventEmitter::new(100);

// Subscribe to all events
let mut receiver = emitter.subscribe();

// Subscribe with filter
let mut started = emitter.subscribe_filtered(|e| e.key == "queue.command.started");

// Consume events in a background task
tokio::spawn(async move {
    while let Ok(event) = receiver.recv().await {
        println!("[{}] {}", event.timestamp, event.key);
    }
});
```

**Lifecycle events emitted automatically:**

| Event key | When emitted | Payload fields |
|-----------|-------------|----------------|
| `queue.command.submitted` | `submit()` accepted | `lane_id` |
| `queue.command.started` | Scheduler dispatched a command | `lane_id`, `command_id`, `command_type` |
| `queue.command.completed` | Command returned `Ok` | `lane_id`, `command_id` |
| `queue.command.retry` | Command failed, will be retried | `lane_id`, `command_id`, `attempt` |
| `queue.command.dead_lettered` | Command moved to DLQ | `lane_id`, `command_id`, `command_type` |
| `queue.command.failed` | Terminal failure (non-timeout) | `lane_id`, `command_id`, `error` |
| `queue.command.timeout` | Terminal failure (timeout) | `lane_id`, `command_id`, `error` |
| `queue.shutdown.started` | `shutdown()` called | *(empty)* |
| `queue.lane.pressure` | *(emitted by AlertManager)* | — |
| `queue.lane.idle` | *(emitted by AlertManager)* | — |

### Reliability Features

#### Command Timeout

```rust
use a3s_lane::{LaneConfig, QueueManagerBuilder, EventEmitter};
use std::time::Duration;

let emitter = EventEmitter::new(100);

// Configure lane with 30-second timeout
let config = LaneConfig::new(1, 5)
    .with_timeout(Duration::from_secs(30));

let manager = QueueManagerBuilder::new(emitter)
    .with_lane("api", config, 0)
    .build()
    .await?;
```

#### Retry Policies

```rust
use a3s_lane::{LaneConfig, RetryPolicy, QueueManagerBuilder, EventEmitter};
use std::time::Duration;

let emitter = EventEmitter::new(100);

// Exponential backoff: 3 retries with 100ms initial delay, 2x multiplier
let retry_policy = RetryPolicy::exponential(3);

// Or fixed delay: 5 retries with 1 second delay
let retry_policy = RetryPolicy::fixed(5, Duration::from_secs(1));

let config = LaneConfig::new(1, 5)
    .with_retry_policy(retry_policy);

let manager = QueueManagerBuilder::new(emitter)
    .with_lane("external-api", config, 0)
    .build()
    .await?;
```

#### Dead Letter Queue

```rust
use a3s_lane::{CommandQueue, EventEmitter, DeadLetterQueue};

let emitter = EventEmitter::new(100);
let dlq = DeadLetterQueue::new(1000); // Max 1000 dead letters

// Create queue with DLQ
let queue = CommandQueue::with_dlq(emitter, dlq.clone());

// Later: inspect failed commands
let dead_letters = dlq.list().await;
for letter in dead_letters {
    println!("Failed: {} - {}", letter.command_type, letter.error);
}
```

#### Graceful Shutdown

```rust
use a3s_lane::QueueManager;
use std::time::Duration;

// Initiate shutdown (stop accepting new commands)
manager.shutdown().await;

// Wait for pending commands to complete (with 30s timeout)
manager.drain(Duration::from_secs(30)).await?;

println!("All commands completed, safe to exit");
```

### Scalability Features

#### Multi-core Parallelism

```rust
use a3s_lane::{PartitionConfig, LocalDistributedQueue};

// Automatically use all CPU cores
let partition_config = PartitionConfig::auto();
let distributed_queue = Arc::new(LocalDistributedQueue::auto());

println!("Using {} CPU cores for parallel processing",
    partition_config.num_partitions);
```

#### Queue Partitioning

```rust
use a3s_lane::{PartitionConfig, PartitionStrategy};

// Round-robin partitioning across 8 workers
let config = PartitionConfig::round_robin(8);

// Hash-based partitioning (same command types go to same partition)
let config = PartitionConfig::hash(8);
```

#### Rate Limiting

Token bucket rate limiting is enforced at dequeue time — commands that exceed the configured rate are held in the queue until a token is available.

```rust
use a3s_lane::{LaneConfig, RateLimitConfig};

// Limit to 100 commands per second (token bucket)
let config = LaneConfig::new(1, 10)
    .with_rate_limit(RateLimitConfig::per_second(100));

// Limit to 1000 commands per minute
let config = LaneConfig::new(1, 10)
    .with_rate_limit(RateLimitConfig::per_minute(1000));
```

#### Priority Boosting

Priority boost is applied on every scheduling tick: the scheduler calls `effective_priority()` on each lane, which reads the front command's submission timestamp and applies the configured boost intervals. A command past its deadline gets priority 0 (highest).

```rust
use a3s_lane::{LaneConfig, PriorityBoostConfig};
use std::time::Duration;

// At 75%/50%/25%/10% of deadline remaining: boost by 1/2/3/4
let config = LaneConfig::new(1, 10)
    .with_priority_boost(PriorityBoostConfig::standard(Duration::from_secs(300)));

// Faster escalation: boost by 2/4/6/8 at 80%/60%/40%/20%
let config = LaneConfig::new(1, 10)
    .with_priority_boost(PriorityBoostConfig::aggressive(Duration::from_secs(60)));
```

#### Custom Distributed Queue

```rust
use a3s_lane::{DistributedQueue, CommandEnvelope, CommandResult};
use async_trait::async_trait;

struct RedisDistributedQueue {
    // Your Redis client
}

#[async_trait]
impl DistributedQueue for RedisDistributedQueue {
    async fn enqueue(&self, envelope: CommandEnvelope) -> Result<()> {
        // Enqueue to Redis
        Ok(())
    }

    async fn dequeue(&self, partition_id: PartitionId) -> Result<Option<CommandEnvelope>> {
        // Dequeue from Redis
        Ok(None)
    }

    // Implement other methods...
}
```

#### Persistent Storage

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, LocalStorage, LaneConfig};
use std::path::PathBuf;

let emitter = EventEmitter::new(100);
let storage_dir = PathBuf::from("./queue_data");
let storage = Arc::new(LocalStorage::new(storage_dir).await?);

// Create queue with persistent storage
let manager = QueueManagerBuilder::new(emitter)
    .with_storage(storage.clone())
    .with_lane("api", LaneConfig::new(1, 5), 0)
    .build()
    .await?;

// Commands are automatically persisted to disk
// On restart, you can inspect stored commands:
let stored_commands = storage.load_commands().await?;
for cmd in stored_commands {
    println!("Pending: {} ({})", cmd.command_type, cmd.id);
}
```

**Custom Storage Backend:**

```rust
use a3s_lane::{Storage, StoredCommand, StoredDeadLetter};
use async_trait::async_trait;

struct RedisStorage {
    // Your Redis client
}

#[async_trait]
impl Storage for RedisStorage {
    async fn save_command(&self, command: StoredCommand) -> Result<()> {
        // Store in Redis
        Ok(())
    }

    async fn load_commands(&self) -> Result<Vec<StoredCommand>> {
        // Load from Redis
        Ok(vec![])
    }

    // Implement other methods...
}
```

### Observability Features

#### Metrics Collection

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, QueueMetrics, LaneConfig};

let emitter = EventEmitter::new(100);

// Create local metrics collector
let metrics = QueueMetrics::local();

// Build queue manager with metrics
let manager = QueueManagerBuilder::new(emitter)
    .with_metrics(metrics.clone())
    .with_lane("api", LaneConfig::new(1, 5), 0)
    .build()
    .await?;

// Record metrics manually (or integrate into command execution)
metrics.record_submit("api").await;
metrics.record_complete("api", 150.0).await; // 150ms latency

// Get metrics snapshot
let snapshot = metrics.snapshot().await;
println!("Commands submitted: {:?}", snapshot.counters.get("lane.commands.submitted"));

// Get latency histogram stats
if let Some(stats) = snapshot.histograms.get("lane.command.latency_ms") {
    println!("Latency p50: {}ms, p99: {}ms", stats.percentiles.p50, stats.percentiles.p99);
}
```

**Custom Metrics Backend (Prometheus/OpenTelemetry):**

```rust
use a3s_lane::{MetricsBackend, HistogramStats, MetricsSnapshot};
use async_trait::async_trait;

struct PrometheusMetrics {
    // Your Prometheus client
}

#[async_trait]
impl MetricsBackend for PrometheusMetrics {
    async fn increment_counter(&self, name: &str, value: u64) {
        // Push to Prometheus
    }

    async fn set_gauge(&self, name: &str, value: f64) {
        // Push to Prometheus
    }

    async fn record_histogram(&self, name: &str, value: f64) {
        // Push to Prometheus
    }

    // Implement other methods...
}
```

#### Queue Depth Alerts

```rust
use a3s_lane::{QueueManagerBuilder, EventEmitter, AlertManager, LaneConfig};
use std::sync::Arc;

let emitter = EventEmitter::new(100);

// Create alert manager with queue depth thresholds
let alerts = Arc::new(AlertManager::with_queue_depth_alerts(
    100,  // Warning threshold
    200,  // Critical threshold
));

// Add alert callback
alerts.add_callback(|alert| {
    println!("[{}] {}: {}",
        match alert.level {
            AlertLevel::Warning => "WARN",
            AlertLevel::Critical => "CRIT",
            _ => "INFO",
        },
        alert.lane_id,
        alert.message
    );
}).await;

// Build queue manager with alerts
let manager = QueueManagerBuilder::new(emitter)
    .with_alerts(alerts.clone())
    .with_lane("api", LaneConfig::new(1, 5), 0)
    .build()
    .await?;

// Check queue depth (triggers alerts if thresholds exceeded)
alerts.check_queue_depth("api", 150).await; // Triggers warning
alerts.check_queue_depth("api", 250).await; // Triggers critical
```

#### Latency Alerts

```rust
use a3s_lane::{AlertManager, LatencyAlertConfig};

// Create alert manager with latency thresholds
let alerts = AlertManager::with_latency_alerts(
    100.0,  // Warning threshold (ms)
    500.0,  // Critical threshold (ms)
);

// Check latency after command execution
alerts.check_latency("api", 250.0).await; // Triggers warning
alerts.check_latency("api", 600.0).await; // Triggers critical
```

## API Reference

### QueueManagerBuilder

| Method | Description |
|--------|-------------|
| `new(emitter)` | Create a new builder |
| `with_lane(id, config, priority)` | Add a custom lane |
| `with_default_lanes()` | Add the 6 default lanes |
| `with_storage(storage)` | Add persistent storage backend |
| `with_dlq(size)` | Add dead letter queue with max size |
| `with_metrics(metrics)` | Add metrics collection |
| `with_alerts(alerts)` | Add alert manager |
| `build()` | Build the QueueManager |

### QueueManager

| Method | Description |
|--------|-------------|
| `start()` | Start the scheduler |
| `submit(lane_id, command)` | Submit a command to a lane |
| `stats()` | Get queue statistics |
| `queue()` | Get underlying CommandQueue |
| `metrics()` | Get metrics collector (if configured) |
| `alerts()` | Get alert manager (if configured) |
| `shutdown()` | Initiate graceful shutdown (stop accepting new commands) |
| `drain(timeout)` | Wait for pending commands to complete with timeout |
| `is_shutting_down()` | Check if shutdown is in progress |

### Lane

| Method | Description |
|--------|-------------|
| `new(id, config, priority)` | Create a new lane |
| `with_storage(id, config, priority, storage)` | Create a lane with persistent storage |
| `id()` | Get lane identifier |
| `priority()` | Get the static base priority |
| `effective_priority()` | Get runtime priority after applying deadline boost (requires `distributed`) |
| `status()` | Get pending / active / min / max counts |

### LaneConfig

| Method | Description |
|--------|-------------|
| `new(min, max)` | Create config with min/max concurrency |
| `with_timeout(duration)` | Set command timeout for this lane |
| `with_retry_policy(policy)` | Set retry policy for this lane |
| `with_rate_limit(config)` | Set rate limiting for this lane (requires `distributed`) |
| `with_priority_boost(config)` | Set priority boosting for this lane (requires `distributed`) |

### RetryPolicy

| Method | Description |
|--------|-------------|
| `exponential(max_retries)` | Create exponential backoff policy (100ms initial, 30s max, 2x multiplier) |
| `fixed(max_retries, delay)` | Create fixed delay policy |
| `none()` | No retries |

### DeadLetterQueue

| Method | Description |
|--------|-------------|
| `new(max_size)` | Create DLQ with maximum size |
| `push(letter)` | Add a dead letter |
| `pop()` | Remove and return oldest dead letter |
| `list()` | Get all dead letters |
| `clear()` | Remove all dead letters |
| `len()` | Get current count |

### Storage Trait

| Method | Description |
|--------|-------------|
| `save_command(cmd)` | Persist a command to storage |
| `load_commands()` | Load all pending commands |
| `remove_command(id)` | Remove a completed command |
| `save_dead_letter(letter)` | Persist a dead letter |
| `load_dead_letters()` | Load all dead letters |
| `clear_dead_letters()` | Clear all dead letters |
| `clear_all()` | Clear all storage |

### LocalStorage

| Method | Description |
|--------|-------------|
| `new(path)` | Create local filesystem storage at path |

### PartitionConfig

| Method | Description |
|--------|-------------|
| `auto()` | Auto-detect CPU cores for optimal parallelism |
| `round_robin(n)` | Round-robin distribution across n partitions |
| `hash(n)` | Hash-based distribution across n partitions |
| `none()` | No partitioning (single partition) |

### RateLimitConfig

| Method | Description |
|--------|-------------|
| `per_second(n)` | Limit to n commands per second |
| `per_minute(n)` | Limit to n commands per minute |
| `per_hour(n)` | Limit to n commands per hour |
| `unlimited()` | No rate limiting |

### PriorityBoostConfig

| Method | Description |
|--------|-------------|
| `standard(deadline)` | Standard boost intervals (25%, 50%, 75%) |
| `aggressive(deadline)` | Aggressive boost intervals |
| `disabled()` | No priority boosting |
| `with_boost(time, boost)` | Add custom boost interval |

### DistributedQueue Trait

| Method | Description |
|--------|-------------|
| `enqueue(envelope)` | Enqueue command for processing |
| `dequeue(partition_id)` | Dequeue command from partition |
| `complete(result)` | Report command completion |
| `num_partitions()` | Get number of partitions |
| `worker_id()` | Get worker identifier |

### LocalDistributedQueue

| Method | Description |
|--------|-------------|
| `auto()` | Create with auto-detected CPU cores |
| `new(config)` | Create with custom partition config |

### QueueMetrics

| Method | Description |
|--------|-------------|
| `local()` | Create with local in-memory backend |
| `new(backend)` | Create with custom metrics backend |
| `record_submit(lane_id)` | Record command submission |
| `record_complete(lane_id, latency_ms)` | Record command completion with latency |
| `record_failure(lane_id)` | Record command failure |
| `record_timeout(lane_id)` | Record command timeout |
| `record_retry(lane_id)` | Record command retry |
| `record_dead_letter(lane_id)` | Record command sent to DLQ |
| `set_queue_depth(lane_id, depth)` | Update queue depth gauge |
| `set_active_commands(lane_id, active)` | Update active commands gauge |
| `record_wait_time(lane_id, wait_time_ms)` | Record command wait time |
| `snapshot()` | Get snapshot of all metrics |
| `reset()` | Reset all metrics |

### MetricsBackend Trait

| Method | Description |
|--------|-------------|
| `increment_counter(name, value)` | Increment a counter metric |
| `set_gauge(name, value)` | Set a gauge metric value |
| `record_histogram(name, value)` | Record histogram observation |
| `get_counter(name)` | Get current counter value |
| `get_gauge(name)` | Get current gauge value |
| `get_histogram_stats(name)` | Get histogram statistics |
| `reset()` | Reset all metrics |
| `snapshot()` | Export all metrics as snapshot |

### AlertManager

| Method | Description |
|--------|-------------|
| `new()` | Create with alerts disabled |
| `with_queue_depth_alerts(warn, crit)` | Create with queue depth alerts |
| `with_latency_alerts(warn_ms, crit_ms)` | Create with latency alerts |
| `set_queue_depth_config(config)` | Update queue depth alert config |
| `set_latency_config(config)` | Update latency alert config |
| `add_callback(callback)` | Add alert callback function |
| `check_queue_depth(lane_id, depth)` | Check queue depth and trigger alerts |
| `check_latency(lane_id, latency_ms)` | Check latency and trigger alerts |

### QueueMonitor

| Method | Description |
|--------|-------------|
| `new(queue)` | Create with default config |
| `with_config(queue, config)` | Create with custom config |
| `start()` | Start background monitoring |
| `stats()` | Get current statistics |

### Command Trait

```rust
#[async_trait]
pub trait Command: Send + Sync {
    /// Execute the command
    async fn execute(&self) -> Result<serde_json::Value>;

    /// Get command type (for logging/debugging)
    fn command_type(&self) -> &str;
}
```

## SDK

### Python

```bash
pip install a3s-lane
```

```python
from a3s_lane import Lane, LaneConfig

# Create and start
lane = Lane({"query": LaneConfig(min_concurrency=1, max_concurrency=10)})
lane.start()

# Submit a command
result = lane.submit("query", "fetch_data", {"url": "https://example.com"})

# Get stats
stats = lane.stats()
print(f"Pending: {stats.total_pending}, Active: {stats.total_active}")

# Shutdown
lane.shutdown()
```

### Node.js

```bash
npm install @a3s-lab/lane
```

```javascript
const { Lane } = require('@a3s-lab/lane');

// Create and start
const lane = new Lane({ query: { minConcurrency: 1, maxConcurrency: 10 } });
lane.start();

// Submit a command
const result = lane.submit('query', 'fetch_data', JSON.stringify({ url: 'https://example.com' }));

// Get stats
const stats = lane.stats();
console.log(`Pending: ${stats.totalPending}, Active: ${stats.totalActive}`);

// Shutdown
lane.shutdown();
```

## Development

### Dependencies

| Dependency | Install | Purpose |
|------------|---------|---------|
| `cargo-llvm-cov` | `cargo install cargo-llvm-cov` | Code coverage (optional) |
| `lcov` | `brew install lcov` / `apt install lcov` | Coverage report formatting (optional) |
| `cargo-watch` | `cargo install cargo-watch` | File watching (optional) |

### Build Commands

```bash
# Build
just build                   # Debug build
just release                 # Release build

# Test (with colored progress display)
just test                    # All tests with pretty output
just test-raw                # Raw cargo output
just test-v                  # Verbose output (--nocapture)
just test-one TEST           # Run specific test

# Test subsets
just test-queue              # Queue module tests
just test-manager            # Manager module tests
just test-monitor            # Monitor module tests
just test-config             # Config module tests
just test-error              # Error module tests
just test-event              # Event module tests

# Coverage (requires cargo-llvm-cov + lcov)
just test-cov                # Pretty coverage with progress
just cov                     # Terminal coverage report
just cov-html                # HTML report (opens in browser)
just cov-table               # File-by-file table
just cov-ci                  # Generate lcov.info for CI
just cov-module queue        # Coverage for specific module

# Format & Lint
just fmt                     # Format code
just fmt-check               # Check formatting
just lint                    # Clippy lint
just ci                      # Full CI checks (fmt + lint + test)

# Utilities
just check                   # Fast compile check
just watch                   # Watch and rebuild
just doc                     # Generate and open docs
just clean                   # Clean build artifacts
just update                  # Update dependencies
```

### Project Structure

```
lane/
├── Cargo.toml
├── justfile
├── README.md
├── CLAUDE.md
├── .github/           # CI/CD workflows
│   ├── setup-workspace.sh
│   └── workflows/
│       ├── ci.yml
│       ├── release.yml
│       ├── publish-node.yml
│       └── publish-python.yml
├── sdk/               # Language SDKs
│   ├── node/          # Node.js SDK (napi-rs)
│   │   ├── Cargo.toml
│   │   ├── package.json
│   │   ├── src/
│   │   └── npm/       # Per-platform packages
│   └── python/        # Python SDK (PyO3)
│       ├── Cargo.toml
│       ├── pyproject.toml
│       └── src/
├── examples/          # Comprehensive feature demonstrations
│   ├── basic_usage.rs
│   ├── reliability.rs
│   ├── observability.rs
│   └── scalability.rs
├── benches/           # Performance benchmarks
│   └── queue_benchmark.rs
└── src/
    ├── lib.rs          # Library entry point with module docs
    ├── config.rs       # LaneConfig with timeout, retry, rate limit
    ├── error.rs        # LaneError and Result types
    ├── event.rs        # EventEmitter, LaneEvent
    ├── queue.rs        # Lane, CommandQueue, Command trait
    ├── manager.rs      # QueueManager, QueueManagerBuilder
    ├── monitor.rs      # QueueMonitor, MonitorConfig
    ├── retry.rs        # RetryPolicy (Phase 2)
    ├── dlq.rs          # DeadLetterQueue (Phase 2)
    ├── storage.rs      # Storage trait, LocalStorage (Phase 2)
    ├── partition.rs    # Partitioning strategies (Phase 3)
    ├── distributed.rs  # Distributed queue support (Phase 3)
    ├── ratelimit.rs    # Rate limiting (Phase 3)
    ├── boost.rs        # Priority boosting (Phase 3)
    ├── metrics.rs      # Metrics collection (Phase 4)
    ├── alerts.rs       # Alert management (Phase 4)
    └── telemetry.rs    # OpenTelemetry integration (Phase 5)
```

## A3S Ecosystem

A3S Lane is the **per-session scheduling layer** of the A3S Agent OS. Each a3s-code agent session gets its own a3s-lane instance, ensuring control commands always preempt LLM generation tasks.

```
┌──────────────────────────────────────────────────────────┐
│                    A3S Agent OS                            │
│                                                            │
│  External:     a3s-gateway  (OS external gateway)          │
│                      │                                     │
│  Sandbox:      a3s-box      (MicroVM isolation)            │
│                      │                                     │
│  Application:  SafeClaw     (OS main app, multi-agent)     │
│                      │                                     │
│  Execution:    a3s-code     (AI agent instances)           │
│                      │                                     │
│  Scheduling:   a3s-lane     (per-session priority queue)   │
│                  ▲                                         │
│                  │ You are here                             │
└──────────────────────────────────────────────────────────┘
```

| Project | Package | Relationship |
|---------|---------|--------------|
| **code** | `a3s-code` | Each a3s-code session creates its own `SessionLaneQueue` wrapping a3s-lane |
| **SafeClaw** | `safeclaw` | Coordinates multiple a3s-code sessions, each with independent a3s-lane instances |

**Standalone Usage**: a3s-lane also works independently for any priority-based async task scheduling:
- Web servers with request prioritization
- Background job processors
- Rate-limited API clients
- Any system needing lane-based concurrency control

## Roadmap

### Shipped ✅

- [x] Priority-based lane scheduling with 6 built-in lanes
- [x] Configurable concurrency, timeout, retry (exponential backoff / fixed delay)
- [x] Dead letter queue, persistent storage (`LocalStorage` + pluggable `Storage` trait)
- [x] Graceful shutdown with configurable drain timeout
- [x] Event system — 11 lifecycle events emitted automatically at every queue stage
- [x] Rate limiting enforced at dequeue time (token bucket, sliding window)
- [x] Priority boosting applied per scheduling tick via `effective_priority()`
- [x] Queue partitioning (round-robin, hash-based, custom strategies)
- [x] Distributed queue support (pluggable `DistributedQueue` trait)
- [x] Metrics collection (`LocalMetrics` + pluggable `MetricsBackend` trait)
- [x] Latency histograms with p50/p90/p95/p99 percentiles
- [x] Queue depth and latency alerts with warning/critical thresholds
- [x] OpenTelemetry OTLP metrics export (`OtelMetricsBackend`)
- [x] Optional feature gates — zero cost for unused subsystems
- [x] Python SDK (PyO3/maturin) — `pip install a3s-lane`
- [x] Node.js SDK (napi-rs) — `npm install @a3s-lab/lane`
- [x] Multi-platform CI/CD — 7 platforms, automated publishing to crates.io / PyPI / npm
- [x] 236 unit tests

### Up Next

- [ ] Async `EventStream` adapter (futures `Stream` impl, no manual recv loop needed)
- [ ] `QueueManager::subscribe()` shortcut (avoid threading `EventEmitter` manually)
- [ ] Priority lane pressure event (auto-emit `queue.lane.pressure` when depth threshold crossed)

## Examples

The `examples/` directory contains comprehensive demonstrations of all features:

### Basic Usage (`examples/basic_usage.rs`)
```bash
cargo run --example basic_usage
```
Demonstrates:
- Creating a queue manager with default lanes
- Submitting commands and handling results
- Checking queue statistics
- Graceful shutdown

### Reliability Features (`examples/reliability.rs`)
```bash
cargo run --example reliability
```
Demonstrates:
- Command timeout configuration
- Retry policies with exponential backoff
- Dead letter queue for failed commands
- Graceful shutdown with drain

### Observability Features (`examples/observability.rs`)
```bash
cargo run --example observability
```
Demonstrates:
- Metrics collection with local backend
- Latency histogram tracking
- Queue depth and latency alerts
- Alert callbacks for notifications

### Scalability Features (`examples/scalability.rs`)
```bash
cargo run --example scalability
```
Demonstrates:
- Rate limiting configuration
- Priority boosting for urgent commands
- Distributed queue with partitioning
- Multi-core parallelism

## Benchmarks

Performance benchmarks are available using Criterion:

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench --bench queue_benchmark

# View HTML reports
open target/criterion/report/index.html
```

**Benchmark suites included:**

1. **Throughput Benchmarking**
   - Tests: 10, 100, 1000 commands
   - Measures: Full lifecycle (create, start, execute, shutdown, drain)
   - Purpose: Understand end-to-end performance including overhead

2. **Concurrency Scaling**
   - Tests: 1, 5, 10, 20 concurrent lanes
   - Measures: 100 commands with simulated work (100μs each)
   - Purpose: Evaluate multi-core scaling efficiency

3. **Priority Scheduling Overhead**
   - Compares: 3 priority lanes vs single lane
   - Measures: 30 commands distributed across priorities
   - Purpose: Quantify cost of priority features

4. **Metrics Collection Overhead**
   - Compares: With vs without metrics
   - Measures: 100 commands with/without observability
   - Purpose: Understand monitoring costs

5. **Rate Limiting**
   - Tests: 100 commands with 50 commands/sec limit
   - Measures: Impact of rate limiting on throughput
   - Purpose: Validate rate limiter behavior

### Sample Results

Benchmarks run on Apple Silicon (M-series) with optimized release build:

| Benchmark | Commands | Time | Throughput |
|-----------|----------|------|------------|
| Full lifecycle | 10 | ~107ms | ~93 ops/sec |
| Full lifecycle | 100 | ~1.17s | ~85 ops/sec |
| Concurrent (1 lane) | 100 | ~10-15ms | ~6,600-10,000 ops/sec |
| Concurrent (10 lanes) | 100 | ~2-3ms | ~33,000-50,000 ops/sec |
| Priority scheduling | 30 | ~3-5ms | ~6,000-10,000 ops/sec |
| With metrics | 100 | ~1.2-1.3s | ~77-83 ops/sec |
| Without metrics | 100 | ~1.1-1.2s | ~83-90 ops/sec |

**Notes:**
- Full lifecycle benchmarks include manager creation, startup, execution, shutdown, and drain
- Concurrent benchmarks measure steady-state throughput with reused manager
- Metrics overhead is approximately 3-5% in typical workloads
- Actual performance varies based on hardware, command complexity, and workload patterns

Results include:
- Mean execution time with 95% confidence intervals
- Throughput measurements (ops/sec)
- Detailed statistical analysis
- Historical comparison charts

## License

MIT
