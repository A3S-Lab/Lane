//! # A3S Lane
//!
//! Lane-based priority command queue for async task scheduling.
//!
//! This library provides a flexible, priority-based command queue system designed
//! for managing concurrent async operations with different priority levels.
//!
//! ## Features
//!
//! - **Priority-based scheduling**: Commands are executed based on lane priority
//! - **Concurrency control**: Per-lane min/max concurrency limits
//! - **Built-in lanes**: 6 predefined lanes (system, control, query, session, skill, prompt)
//! - **Event system**: Subscribe to queue events for monitoring
//! - **Health monitoring**: Track queue depth and active command counts
//! - **Builder pattern**: Flexible queue configuration
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use a3s_lane::{QueueManagerBuilder, EventEmitter};
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
//!     // Get statistics
//!     let stats = manager.stats().await?;
//!     println!("Pending: {}, Active: {}", stats.total_pending, stats.total_active);
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Lane Priorities
//!
//! | Lane | Priority | Default Max Concurrency |
//! |------|----------|------------------------|
//! | system | 0 (highest) | 5 |
//! | control | 1 | 3 |
//! | query | 2 | 10 |
//! | session | 3 | 5 |
//! | skill | 4 | 3 |
//! | prompt | 5 (lowest) | 2 |

pub mod boost;
pub mod config;
pub mod distributed;
pub mod dlq;
pub mod error;
pub mod event;
pub mod manager;
pub mod monitor;
pub mod partition;
pub mod queue;
pub mod ratelimit;
pub mod retry;
pub mod storage;

// Re-export main types
pub use boost::{PriorityBoostConfig, PriorityBooster};
pub use config::LaneConfig;
pub use distributed::{
    CommandEnvelope, CommandResult, DistributedQueue, LocalDistributedQueue, WorkerPool, WorkerId,
};
pub use dlq::{DeadLetter, DeadLetterQueue};
pub use error::{LaneError, Result};
pub use event::{EventEmitter, EventPayload, EventStream, LaneEvent};
pub use manager::{QueueManager, QueueManagerBuilder};
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

/// Queue statistics
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
