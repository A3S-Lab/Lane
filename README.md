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

// Exponential backoff: 3 retries, 1s -> 2s -> 4s
let retry_policy = RetryPolicy::exponential(3)
    .with_initial_delay(Duration::from_secs(1))
    .with_max_delay(Duration::from_secs(10));

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

## API Reference

### QueueManagerBuilder

| Method | Description |
|--------|-------------|
| `new(emitter)` | Create a new builder |
| `with_lane(id, config, priority)` | Add a custom lane |
| `with_default_lanes()` | Add the 6 default lanes |
| `with_storage(storage)` | Add persistent storage backend |
| `with_dlq(size)` | Add dead letter queue with max size |
| `build()` | Build the QueueManager |

### QueueManager

| Method | Description |
|--------|-------------|
| `start()` | Start the scheduler |
| `submit(lane_id, command)` | Submit a command to a lane |
| `stats()` | Get queue statistics |
| `queue()` | Get underlying CommandQueue |
| `shutdown()` | Initiate graceful shutdown (stop accepting new commands) |
| `drain(timeout)` | Wait for pending commands to complete with timeout |
| `is_shutting_down()` | Check if shutdown is in progress |

### LaneConfig

| Method | Description |
|--------|-------------|
| `new(min, max)` | Create config with min/max concurrency |
| `with_timeout(duration)` | Set command timeout for this lane |
| `with_retry_policy(policy)` | Set retry policy for this lane |

### RetryPolicy

| Method | Description |
|--------|-------------|
| `exponential(max_retries)` | Create exponential backoff policy (2x multiplier) |
| `fixed(max_retries, delay)` | Create fixed delay policy |
| `none()` | No retries |
| `with_initial_delay(duration)` | Set initial delay (default: 1s) |
| `with_max_delay(duration)` | Set maximum delay cap (default: 60s) |
| `with_multiplier(f64)` | Set backoff multiplier (default: 2.0) |

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
└── src/
    ├── lib.rs          # Library entry point
    ├── config.rs       # LaneConfig
    ├── error.rs        # LaneError
    ├── event.rs        # EventEmitter, LaneEvent
    ├── queue.rs        # Lane, CommandQueue, Command trait
    ├── manager.rs      # QueueManager, QueueManagerBuilder
    ├── monitor.rs      # QueueMonitor, MonitorConfig
    ├── retry.rs        # RetryPolicy (Phase 2)
    ├── dlq.rs          # DeadLetterQueue (Phase 2)
    └── storage.rs      # Storage trait, LocalStorage (Phase 2)
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

### Phase 1: Core ✅

- [x] Priority-based lane scheduling
- [x] Configurable concurrency per lane
- [x] Event system for monitoring
- [x] Queue manager with builder pattern
- [x] Health monitoring with thresholds
- [x] Async-first with Tokio

### Phase 2: Reliability ✅

- [x] Persistent queue storage (LocalStorage + pluggable Storage trait)
- [x] Command timeout support
- [x] Command retries with exponential backoff
- [x] Dead letter queue for failed commands
- [x] Graceful shutdown with drain

### Phase 3: Scalability 📋

- [ ] Distributed queue support
- [ ] Priority boosting (deadline-based)
- [ ] Rate limiting per lane
- [ ] Queue partitioning

### Phase 4: Observability 📋

- [ ] Prometheus metrics export
- [ ] OpenTelemetry integration
- [ ] Queue depth alerts
- [ ] Latency histograms

## License

MIT
