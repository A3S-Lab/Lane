//! # A3S Lane
//!
//! A high-performance, priority-based command queue for async task scheduling with comprehensive
//! reliability, scalability, and observability features.
//!
//! ## Overview
//!
//! A3S Lane provides a lane-based priority command queue system designed for managing concurrent
//! async operations with different priority levels. Commands are organized into lanes, each with
//! configurable concurrency limits, timeouts, retry policies, and rate limiting.
//!
//! ## Core Features
//!
//! ### Phase 1: Core Queue System
//! - **Priority-based scheduling**: Commands execute based on lane priority (lower = higher priority)
//! - **Concurrency control**: Per-lane min/max concurrency limits with semaphore-based coordination
//! - **Built-in lanes**: 6 predefined lanes (system, control, query, session, skill, prompt)
//! - **Event system**: Subscribe to queue events for real-time monitoring
//! - **Health monitoring**: Track queue depth and active command counts
//! - **Builder pattern**: Flexible, ergonomic queue configuration
//!
//! ### Phase 2: Reliability
//! - **Command timeout**: Configurable timeout per lane with automatic cancellation
//! - **Retry policies**: Exponential backoff, fixed delay, or custom retry strategies
//! - **Dead letter queue**: Capture permanently failed commands for inspection and replay
//! - **Persistent storage**: Optional pluggable storage backend (LocalStorage included)
//! - **Graceful shutdown**: Drain pending commands before shutdown with timeout
//!
//! ### Phase 3: Scalability
//! - **Multi-core parallelism**: Automatic CPU core detection and parallel processing
//! - **Queue partitioning**: Distribute commands across workers (round-robin, hash-based, custom)
//! - **Rate limiting**: Token bucket and sliding window rate limiters per lane
//! - **Priority boosting**: Deadline-based automatic priority adjustment
//! - **Distributed queue**: Pluggable interface for multi-machine processing
//!
//! ### Phase 4: Observability
//! - **Metrics collection**: Local in-memory metrics with pluggable backend support
//! - **Latency histograms**: Track command execution and wait time with percentiles (p50, p90, p95, p99)
//! - **Queue depth alerts**: Configurable warning and critical thresholds
//! - **Latency alerts**: Monitor and alert on command execution latency
//! - **Prometheus/OpenTelemetry ready**: Implement MetricsBackend trait for external systems
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use a3s_lane::{QueueManagerBuilder, EventEmitter, Command, Result};
//! use async_trait::async_trait;
//!
//! // Define a command
//! struct MyCommand {
//!     data: String,
//! }
//!
//! #[async_trait]
//! impl Command for MyCommand {
//!     async fn execute(&self) -> Result<serde_json::Value> {
//!         Ok(serde_json::json!({"processed": self.data}))
//!     }
//!
//!     fn command_type(&self) -> &str {
//!         "my_command"
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Create event emitter
//!     let emitter = EventEmitter::new(100);
//!
//!     // Build queue manager with default lanes
//!     let manager = QueueManagerBuilder::new(emitter)
//!         .with_default_lanes()
//!         .build()
//!         .await?;
//!
//!     // Start the scheduler
//!     manager.start().await?;
//!
//!     // Submit a command
//!     let cmd = Box::new(MyCommand { data: "hello".to_string() });
//!     let rx = manager.submit("query", cmd).await?;
//!
//!     // Wait for result
//!     let result = rx.await??;
//!     println!("Result: {}", result);
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Lane Priority Model
//!
//! | Lane | Priority | Default Max Concurrency | Use Case |
//! |------|----------|------------------------|----------|
//! | system | 0 (highest) | 5 | System-level operations |
//! | control | 1 | 3 | Control commands (pause, resume, cancel) |
//! | query | 2 | 10 | Read-only queries |
//! | session | 3 | 5 | Session management |
//! | skill | 4 | 3 | Skill/tool execution |
//! | prompt | 5 (lowest) | 2 | LLM prompt processing |
//!
//! ## Examples
//!
//! See the `examples/` directory for comprehensive examples:
//! - `basic_usage.rs` - Simple command submission and result handling
//! - `reliability.rs` - Timeout, retry policies, DLQ, graceful shutdown
//! - `observability.rs` - Metrics collection, latency histograms, alerts
//! - `scalability.rs` - Rate limiting, priority boosting, partitioning

pub mod alerts;
pub mod boost;
pub mod config;
pub mod distributed;
pub mod dlq;
pub mod error;
pub mod event;
pub mod manager;
pub mod metrics;
pub mod monitor;
pub mod partition;
pub mod queue;
pub mod ratelimit;
pub mod retry;
pub mod storage;

// Re-export main types
pub use alerts::{Alert, AlertLevel, AlertManager, LatencyAlertConfig, QueueDepthAlertConfig};
pub use boost::{PriorityBoostConfig, PriorityBooster};
pub use config::LaneConfig;
pub use distributed::{
    CommandEnvelope, CommandResult, DistributedQueue, LocalDistributedQueue, WorkerPool, WorkerId,
};
pub use dlq::{DeadLetter, DeadLetterQueue};
pub use error::{LaneError, Result};
pub use event::{EventEmitter, EventPayload, EventStream, LaneEvent};
pub use manager::{QueueManager, QueueManagerBuilder};
pub use metrics::{
    metric_names, HistogramPercentiles, HistogramStats, LocalMetrics, MetricsBackend,
    MetricsSnapshot, QueueMetrics,
};
pub use monitor::{MonitorConfig, QueueMonitor};
pub use partition::{
    CustomPartitioner, HashPartitioner, PartitionConfig, PartitionId, PartitionStrategy,
    Partitioner, RoundRobinPartitioner,
};
pub use queue::{
    lane_ids, priorities, Command, CommandId, CommandQueue, Lane, LaneId, LaneStatus, Priority,
};
pub use ratelimit::{RateLimitConfig, RateLimiter, SlidingWindowLimiter, TokenBucketLimiter};
pub use retry::RetryPolicy;
pub use storage::{LocalStorage, Storage, StoredCommand, StoredDeadLetter};

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Queue statistics snapshot
///
/// Provides a point-in-time view of the queue state across all lanes.
///
/// # Fields
///
/// * `total_pending` - Total number of commands waiting to be executed across all lanes
/// * `total_active` - Total number of commands currently executing across all lanes
/// * `dead_letter_count` - Total number of permanently failed commands in the dead letter queue
/// * `lanes` - Per-lane status information (pending, active, min/max concurrency)
///
/// # Example
///
/// ```rust,ignore
/// let stats = manager.stats().await?;
/// println!("Queue has {} pending and {} active commands",
///     stats.total_pending, stats.total_active);
///
/// for (lane_id, status) in &stats.lanes {
///     println!("{}: {} pending, {} active (max: {})",
///         lane_id, status.pending, status.active, status.max);
/// }
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueStats {
    pub total_pending: usize,
    pub total_active: usize,
    pub dead_letter_count: usize,
    pub lanes: HashMap<String, LaneStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_queue_manager_builder() {
        let emitter = EventEmitter::new(100);
        let manager = QueueManagerBuilder::new(emitter)
            .with_default_lanes()
            .build()
            .await
            .unwrap();

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.lanes.len(), 6);
    }

    #[test]
    fn test_queue_stats_default() {
        let stats = QueueStats::default();
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
        assert!(stats.lanes.is_empty());
    }

    #[test]
    fn test_queue_stats_serialization() {
        let mut lanes = HashMap::new();
        lanes.insert(
            "query".to_string(),
            LaneStatus {
                pending: 5,
                active: 2,
                min: 1,
                max: 10,
            },
        );

        let stats = QueueStats {
            total_pending: 5,
            total_active: 2,
            dead_letter_count: 0,
            lanes,
        };

        let json = serde_json::to_string(&stats).unwrap();
        let parsed: QueueStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_pending, 5);
        assert_eq!(parsed.total_active, 2);
    }
}
