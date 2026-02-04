# A3S Lane

<p align="center">
  <strong>Priority-Based Command Queue</strong>
</p>

<p align="center">
  <em>Utility layer — lane-based async task scheduling with configurable concurrency</em>
</p>

<p align="center">
  <a href="#features">Features</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#api-reference">API Reference</a> •
  <a href="#development">Development</a>
</p>

---

## Overview

**A3S Lane** provides a lane-based priority command queue designed for managing concurrent async operations with different priority levels. Commands are organized into lanes, each with configurable concurrency limits and priority.

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
async fn main() -> anyhow::Result<()> {
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

- **Priority-based Scheduling**: Commands execute based on lane priority (lower = higher priority)
- **Concurrency Control**: Per-lane min/max concurrency limits
- **6 Built-in Lanes**: system, control, query, session, skill, prompt
- **Command Timeout**: Configurable timeout per lane with automatic cancellation
- **Retry Policies**: Exponential backoff, fixed delay, or custom retry strategies
- **Dead Letter Queue**: Capture permanently failed commands for inspection
- **Persistent Storage**: Optional pluggable storage backend (LocalStorage included)
- **Graceful Shutdown**: Drain pending commands before shutdown
- **Multi-core Parallelism**: Automatic CPU core detection and parallel processing
- **Queue Partitioning**: Distribute commands across workers for scalability
- **Rate Limiting**: Token bucket and sliding window rate limiters per lane
- **Priority Boosting**: Deadline-based automatic priority adjustment
- **Distributed Queue Support**: Pluggable interface for multi-machine processing
- **Metrics Collection**: Local in-memory metrics with pluggable backend support
- **Latency Histograms**: Track command execution and wait time with percentiles (p50, p90, p95, p99)
- **Queue Depth Alerts**: Configurable warning and critical thresholds
- **Latency Alerts**: Monitor and alert on command execution latency
- **Event System**: Subscribe to queue events for monitoring
- **Health Monitoring**: Track queue depth and active command counts
- **Builder Pattern**: Flexible queue configuration
- **Async-first**: Built on Tokio for high-performance async operations

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

Add to your `Cargo.toml`:

```toml
[dependencies]
a3s-lane = "0.1"
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

```rust
use a3s_lane::EventEmitter;

let emitter = EventEmitter::new(100);

// Subscribe to all events
let mut receiver = emitter.subscribe();

// Subscribe with filter
let mut filtered = emitter.subscribe_filtered(|e| e.key.starts_with("queue."));

// In another task
tokio::spawn(async move {
    while let Ok(event) = receiver.recv().await {
        println!("Event: {} at {}", event.key, event.timestamp);
    }
});
```

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

```rust
use a3s_lane::{LaneConfig, RateLimitConfig};

// Limit to 100 commands per second
let rate_limit = RateLimitConfig::per_second(100);
let config = LaneConfig::new(1, 10)
    .with_rate_limit(rate_limit);

// Limit to 1000 commands per minute
let rate_limit = RateLimitConfig::per_minute(1000);
let config = LaneConfig::new(1, 10)
    .with_rate_limit(rate_limit);
```

#### Priority Boosting

```rust
use a3s_lane::{LaneConfig, PriorityBoostConfig};
use std::time::Duration;

// Standard boost: priority increases as deadline approaches
let boost = PriorityBoostConfig::standard(Duration::from_secs(300));
let config = LaneConfig::new(1, 10)
    .with_priority_boost(boost);

// Aggressive boost: faster priority escalation
let boost = PriorityBoostConfig::aggressive(Duration::from_secs(60));
let config = LaneConfig::new(1, 10)
    .with_priority_boost(boost);
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

### LaneConfig

| Method | Description |
|--------|-------------|
| `new(min, max)` | Create config with min/max concurrency |
| `with_timeout(duration)` | Set command timeout for this lane |
| `with_retry_policy(policy)` | Set retry policy for this lane |
| `with_rate_limit(config)` | Set rate limiting for this lane |
| `with_priority_boost(config)` | Set priority boosting for this lane |

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
    └── alerts.rs       # Alert management (Phase 4)
```

## A3S Ecosystem

A3S Lane is a **utility component** of the A3S ecosystem — a standalone priority queue that can be used by any async Rust application.

```
┌──────────────────────────────────────────────────────────┐
│                    A3S Ecosystem                         │
│                                                          │
│  Infrastructure:  a3s-box     (MicroVM sandbox runtime)  │
│                      │                                   │
│  Application:     a3s-code    (AI coding agent)          │
│                    /   \                                 │
│  Utilities:   a3s-lane  a3s-context                     │
│                  ▲      (memory/knowledge)               │
│                  │                                       │
│            You are here                                  │
└──────────────────────────────────────────────────────────┘
```

| Project | Package | Relationship |
|---------|---------|--------------|
| **box** | `a3s-box-*` | Can use `lane` for internal task scheduling |
| **code** | `a3s-code` | Uses `lane` for command priority and concurrency control |
| **context** | `a3s-context` | Independent utility (no direct relationship) |

**Standalone Usage**: `a3s-lane` works independently for any priority-based async task scheduling:
- Web servers with request prioritization
- Background job processors
- Rate-limited API clients
- Any system needing lane-based concurrency control

## Roadmap

### Phase 1: Core ✅ (Complete)

- [x] Priority-based lane scheduling
- [x] Configurable concurrency per lane
- [x] Event system for monitoring
- [x] Queue manager with builder pattern
- [x] Health monitoring with thresholds
- [x] Async-first with Tokio
- [x] 212 comprehensive tests

### Phase 2: Reliability ✅ (Complete)

- [x] Persistent queue storage (LocalStorage + pluggable Storage trait)
- [x] Command timeout support with automatic cancellation
- [x] Command retries with exponential backoff and fixed delay strategies
- [x] Dead letter queue for permanently failed commands
- [x] Graceful shutdown with drain and timeout

### Phase 3: Scalability ✅ (Complete)

- [x] Queue partitioning (round-robin, hash-based, custom strategies)
- [x] Multi-core parallelism (automatic CPU core detection)
- [x] Distributed queue support (pluggable DistributedQueue trait)
- [x] Priority boosting (deadline-based automatic priority adjustment)
- [x] Rate limiting per lane (token bucket and sliding window algorithms)

### Phase 4: Observability ✅ (Complete)

- [x] Metrics collection (LocalMetrics + pluggable MetricsBackend trait)
- [x] Latency histograms with percentiles (p50, p90, p95, p99)
- [x] Queue depth alerts with configurable warning/critical thresholds
- [x] Latency alerts with warning and critical levels
- [x] Prometheus/OpenTelemetry ready (implement MetricsBackend trait)
- [x] Alert callbacks for custom notification handling

### Documentation & Examples ✅ (Complete)

- [x] Comprehensive README with all features documented
- [x] 4 working examples demonstrating all major features
- [x] Performance benchmarks with Criterion
- [x] Detailed API reference
- [x] Inline documentation for all public APIs

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
```

Benchmarks include:
- **Throughput**: 1K to 100K commands
- **Concurrency scaling**: 1 to 16 lanes
- **Priority scheduling overhead**: Comparison with and without priorities
- **Metrics overhead**: Impact of metrics collection on performance

## License

MIT
