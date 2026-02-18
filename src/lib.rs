//! # A3S Lane
//!
//! A priority-based command queue for async task scheduling.
//!
//! ## Core (always compiled)
//!
//! - Priority-based scheduling with per-lane concurrency control
//! - Command timeout and retry policies (exponential backoff, fixed delay)
//! - Dead letter queue for permanently failed commands
//! - Persistent storage (pluggable `Storage` trait, `LocalStorage` included)
//! - Event system for queue lifecycle notifications
//! - Graceful shutdown with drain support
//!
//! ## Feature Flags
//!
//! | Feature | Default | Dependencies | Description |
//! |---------|---------|-------------|-------------|
//! | `metrics` | ✅ | — | `MetricsBackend` trait, `LocalMetrics`, latency histograms |
//! | `monitoring` | ✅ | `metrics` | `AlertManager`, `QueueMonitor` with depth/latency thresholds |
//! | `telemetry` | ✅ | `opentelemetry`, `dashmap` | OpenTelemetry spans and `OtelMetricsBackend` |
//! | `distributed` | ✅ | `num_cpus` | Partitioning, rate limiting, priority boosting, `DistributedQueue` |
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use a3s_lane::{QueueManagerBuilder, EventEmitter, Command, Result};
//! use async_trait::async_trait;
//!
//! struct MyCommand { data: String }
//!
//! #[async_trait]
//! impl Command for MyCommand {
//!     async fn execute(&self) -> Result<serde_json::Value> {
//!         Ok(serde_json::json!({"processed": self.data}))
//!     }
//!     fn command_type(&self) -> &str { "my_command" }
//! }
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     let manager = QueueManagerBuilder::new(EventEmitter::new(100))
//!         .with_default_lanes()
//!         .build()
//!         .await?;
//!
//!     manager.start().await?;
//!
//!     let rx = manager.submit("query", Box::new(MyCommand { data: "hello".into() })).await?;
//!     let result = rx.await??;
//!     println!("Result: {}", result);
//!     Ok(())
//! }
//! ```

// Core modules (always compiled)
pub mod config;
pub mod dlq;
pub mod error;
pub mod event;
pub mod manager;
pub mod queue;
pub mod retry;
pub mod storage;

// Feature-gated modules
#[cfg(feature = "metrics")]
pub mod metrics;
#[cfg(feature = "monitoring")]
pub mod alerts;
#[cfg(feature = "monitoring")]
pub mod monitor;
#[cfg(feature = "telemetry")]
pub mod telemetry;
#[cfg(feature = "distributed")]
pub mod boost;
#[cfg(feature = "distributed")]
pub mod partition;
#[cfg(feature = "distributed")]
pub mod ratelimit;
#[cfg(feature = "distributed")]
pub mod distributed;

// Core re-exports
pub use config::LaneConfig;
pub use dlq::{DeadLetter, DeadLetterQueue};
pub use error::{LaneError, Result};
pub use event::{EventEmitter, EventPayload, EventStream, LaneEvent};
pub use manager::{QueueManager, QueueManagerBuilder};
pub use queue::{
    lane_ids, priorities, Command, CommandId, CommandQueue, JsonCommand, Lane, LaneId, LaneStatus,
    Priority,
};
pub use retry::RetryPolicy;
pub use storage::{LocalStorage, Storage, StoredCommand, StoredDeadLetter};

// Feature-gated re-exports
#[cfg(feature = "metrics")]
pub use metrics::{
    metric_names, HistogramPercentiles, HistogramStats, LocalMetrics, MetricsBackend,
    MetricsSnapshot, QueueMetrics,
};
#[cfg(feature = "monitoring")]
pub use alerts::{Alert, AlertLevel, AlertManager, LatencyAlertConfig, QueueDepthAlertConfig};
#[cfg(feature = "monitoring")]
pub use monitor::{MonitorConfig, QueueMonitor};
#[cfg(feature = "telemetry")]
pub use telemetry::OtelMetricsBackend;
#[cfg(feature = "distributed")]
pub use boost::{PriorityBoostConfig, PriorityBooster};
#[cfg(feature = "distributed")]
pub use distributed::{
    CommandEnvelope, CommandResult, DistributedQueue, LocalDistributedQueue, WorkerId, WorkerPool,
};
#[cfg(feature = "distributed")]
pub use partition::{
    CustomPartitioner, HashPartitioner, PartitionConfig, PartitionId, PartitionStrategy,
    Partitioner, RoundRobinPartitioner,
};
#[cfg(feature = "distributed")]
pub use ratelimit::{RateLimitConfig, RateLimiter, SlidingWindowLimiter, TokenBucketLimiter};

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
