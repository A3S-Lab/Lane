# A3S Lane

<p align="center">
  <strong>Lane-based Priority Command Queue for Async Task Scheduling</strong>
</p>

<p align="center">
  <em>Flexible, priority-based command queue system for managing concurrent async operations</em>
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

## API Reference

### QueueManagerBuilder

| Method | Description |
|--------|-------------|
| `new(emitter)` | Create a new builder |
| `with_lane(id, config, priority)` | Add a custom lane |
| `with_default_lanes()` | Add the 6 default lanes |
| `build()` | Build the QueueManager |

### QueueManager

| Method | Description |
|--------|-------------|
| `start()` | Start the scheduler |
| `submit(lane_id, command)` | Submit a command to a lane |
| `stats()` | Get queue statistics |
| `queue()` | Get underlying CommandQueue |

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
    └── monitor.rs      # QueueMonitor, MonitorConfig
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

### Phase 2: Reliability 🚧

- [ ] Persistent queue storage
- [ ] Command retries with exponential backoff
- [ ] Dead letter queue for failed commands
- [ ] Graceful shutdown with drain

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
