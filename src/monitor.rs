//! Queue monitor for tracking queue metrics and health

use crate::queue::CommandQueue;
use crate::QueueStats;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Queue monitor configuration
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Monitoring interval
    pub interval: Duration,
    /// Warning threshold for pending commands
    pub pending_warning_threshold: usize,
    /// Warning threshold for active commands
    pub active_warning_threshold: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            pending_warning_threshold: 100,
            active_warning_threshold: 50,
        }
    }
}

/// Queue monitor
pub struct QueueMonitor {
    queue: Arc<CommandQueue>,
    config: MonitorConfig,
}

impl QueueMonitor {
    /// Create a new queue monitor
    pub fn new(queue: Arc<CommandQueue>) -> Self {
        Self::with_config(queue, MonitorConfig::default())
    }

    /// Create a new queue monitor with custom configuration
    pub fn with_config(queue: Arc<CommandQueue>, config: MonitorConfig) -> Self {
        Self { queue, config }
    }

    /// Start monitoring
    pub async fn start(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(self.config.interval);

        tokio::spawn(async move {
            loop {
                ticker.tick().await;
                self.check_health().await;
            }
        });
    }

    /// Check queue health
    async fn check_health(&self) {
        let status = self.queue.status().await;

        let mut total_pending = 0;
        let mut total_active = 0;

        for (lane_id, lane_status) in status.iter() {
            total_pending += lane_status.pending;
            total_active += lane_status.active;

            debug!(
                "Lane {}: pending={}, active={}, max={}",
                lane_id, lane_status.pending, lane_status.active, lane_status.max
            );

            // Check if lane is at capacity
            if lane_status.active >= lane_status.max {
                warn!("Lane {} is at maximum capacity", lane_id);
            }
        }

        // Check global thresholds
        if total_pending > self.config.pending_warning_threshold {
            warn!(
                "High number of pending commands: {} (threshold: {})",
                total_pending, self.config.pending_warning_threshold
            );
        }

        if total_active > self.config.active_warning_threshold {
            warn!(
                "High number of active commands: {} (threshold: {})",
                total_active, self.config.active_warning_threshold
            );
        }
    }

    /// Get current statistics
    pub async fn stats(&self) -> QueueStats {
        let lane_status = self.queue.status().await;

        let mut total_pending = 0;
        let mut total_active = 0;

        for status in lane_status.values() {
            total_pending += status.pending;
            total_active += status.active;
        }

        let dead_letter_count = match self.queue.dlq() {
            Some(dlq) => dlq.len().await,
            None => 0,
        };

        QueueStats {
            total_pending,
            total_active,
            dead_letter_count,
            lanes: lane_status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LaneConfig;
    use crate::error::Result;
    use crate::event::EventEmitter;
    use crate::queue::{lane_ids, priorities, Command, Lane};
    use async_trait::async_trait;

    /// Helper: create a CommandQueue with default lanes registered
    async fn make_queue_with_lanes() -> Arc<CommandQueue> {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let lanes = [
            (lane_ids::SYSTEM, priorities::SYSTEM, 5),
            (lane_ids::CONTROL, priorities::CONTROL, 3),
            (lane_ids::QUERY, priorities::QUERY, 10),
            (lane_ids::PROMPT, priorities::PROMPT, 2),
        ];

        for (id, priority, max) in lanes {
            let config = LaneConfig::new(1, max);
            queue
                .register_lane(Arc::new(Lane::new(id, config, priority)))
                .await;
        }

        queue
    }

    struct TestCommand {
        result: serde_json::Value,
    }

    #[async_trait]
    impl Command for TestCommand {
        async fn execute(&self) -> Result<serde_json::Value> {
            Ok(self.result.clone())
        }
        fn command_type(&self) -> &str {
            "test"
        }
    }

    // ========================================================================
    // MonitorConfig Tests
    // ========================================================================

    #[test]
    fn test_monitor_config_default() {
        let config = MonitorConfig::default();
        assert_eq!(config.interval, Duration::from_secs(10));
        assert_eq!(config.pending_warning_threshold, 100);
        assert_eq!(config.active_warning_threshold, 50);
    }

    #[test]
    fn test_monitor_config_custom() {
        let config = MonitorConfig {
            interval: Duration::from_secs(5),
            pending_warning_threshold: 50,
            active_warning_threshold: 20,
        };
        assert_eq!(config.interval, Duration::from_secs(5));
        assert_eq!(config.pending_warning_threshold, 50);
        assert_eq!(config.active_warning_threshold, 20);
    }

    #[test]
    fn test_monitor_config_clone() {
        let config = MonitorConfig {
            interval: Duration::from_millis(500),
            pending_warning_threshold: 10,
            active_warning_threshold: 5,
        };
        let cloned = config.clone();
        assert_eq!(cloned.interval, Duration::from_millis(500));
        assert_eq!(cloned.pending_warning_threshold, 10);
        assert_eq!(cloned.active_warning_threshold, 5);
    }

    #[test]
    fn test_monitor_config_debug() {
        let config = MonitorConfig::default();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("MonitorConfig"));
        assert!(debug_str.contains("interval"));
        assert!(debug_str.contains("pending_warning_threshold"));
        assert!(debug_str.contains("active_warning_threshold"));
    }

    // ========================================================================
    // QueueMonitor Construction Tests
    // ========================================================================

    #[tokio::test]
    async fn test_monitor_new_default_config() {
        let queue = make_queue_with_lanes().await;
        let monitor = QueueMonitor::new(Arc::clone(&queue));
        assert_eq!(monitor.config.interval, Duration::from_secs(10));
        assert_eq!(monitor.config.pending_warning_threshold, 100);
        assert_eq!(monitor.config.active_warning_threshold, 50);
    }

    #[tokio::test]
    async fn test_monitor_with_config() {
        let queue = make_queue_with_lanes().await;
        let config = MonitorConfig {
            interval: Duration::from_secs(1),
            pending_warning_threshold: 10,
            active_warning_threshold: 5,
        };
        let monitor = QueueMonitor::with_config(Arc::clone(&queue), config);
        assert_eq!(monitor.config.interval, Duration::from_secs(1));
        assert_eq!(monitor.config.pending_warning_threshold, 10);
        assert_eq!(monitor.config.active_warning_threshold, 5);
    }

    // ========================================================================
    // stats() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_monitor_stats_empty_queue() {
        let queue = make_queue_with_lanes().await;
        let monitor = QueueMonitor::new(queue);

        let stats = monitor.stats().await;
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
        assert_eq!(stats.lanes.len(), 4);
    }

    #[tokio::test]
    async fn test_monitor_stats_with_pending_commands() {
        let queue = make_queue_with_lanes().await;

        // Enqueue commands (without starting scheduler, they stay pending)
        for _ in 0..3 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = queue.submit(lane_ids::QUERY, cmd).await;
        }
        for _ in 0..2 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = queue.submit(lane_ids::PROMPT, cmd).await;
        }

        let monitor = QueueMonitor::new(queue);
        let stats = monitor.stats().await;

        assert_eq!(stats.total_pending, 5);
        assert_eq!(stats.total_active, 0);
        assert_eq!(stats.lanes[lane_ids::QUERY].pending, 3);
        assert_eq!(stats.lanes[lane_ids::PROMPT].pending, 2);
        assert_eq!(stats.lanes[lane_ids::SYSTEM].pending, 0);
        assert_eq!(stats.lanes[lane_ids::CONTROL].pending, 0);
    }

    #[tokio::test]
    async fn test_monitor_stats_lane_concurrency() {
        let queue = make_queue_with_lanes().await;
        let monitor = QueueMonitor::new(queue);

        let stats = monitor.stats().await;
        assert_eq!(stats.lanes[lane_ids::SYSTEM].max, 5);
        assert_eq!(stats.lanes[lane_ids::CONTROL].max, 3);
        assert_eq!(stats.lanes[lane_ids::QUERY].max, 10);
        assert_eq!(stats.lanes[lane_ids::PROMPT].max, 2);
    }

    #[tokio::test]
    async fn test_monitor_stats_no_lanes() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));
        let monitor = QueueMonitor::new(queue);

        let stats = monitor.stats().await;
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
        assert!(stats.lanes.is_empty());
    }

    // ========================================================================
    // check_health() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_check_health_no_warnings() {
        let queue = make_queue_with_lanes().await;
        let monitor = QueueMonitor::new(queue);

        // Should not panic or error with empty queue
        monitor.check_health().await;
    }

    #[tokio::test]
    async fn test_check_health_with_pending_below_threshold() {
        let queue = make_queue_with_lanes().await;

        // Add a few commands (below default threshold of 100)
        for _ in 0..5 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = queue.submit(lane_ids::QUERY, cmd).await;
        }

        let monitor = QueueMonitor::new(queue);

        // Should complete without errors
        monitor.check_health().await;
    }

    #[tokio::test]
    async fn test_check_health_with_pending_above_threshold() {
        let queue = make_queue_with_lanes().await;

        // Set a low threshold and add commands above it
        let config = MonitorConfig {
            interval: Duration::from_secs(10),
            pending_warning_threshold: 3,
            active_warning_threshold: 50,
        };

        // Add 5 commands (above threshold of 3)
        for _ in 0..5 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = queue.submit(lane_ids::QUERY, cmd).await;
        }

        let monitor = QueueMonitor::with_config(queue, config);

        // Should not panic, just emit warning log
        monitor.check_health().await;
    }

    #[tokio::test]
    async fn test_check_health_at_exact_threshold() {
        let queue = make_queue_with_lanes().await;

        let config = MonitorConfig {
            interval: Duration::from_secs(10),
            pending_warning_threshold: 3,
            active_warning_threshold: 50,
        };

        // Add exactly 3 commands (equal to threshold, should NOT trigger warning)
        for _ in 0..3 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = queue.submit(lane_ids::QUERY, cmd).await;
        }

        let monitor = QueueMonitor::with_config(queue, config);
        monitor.check_health().await;
    }

    // ========================================================================
    // start() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_monitor_start_runs_periodically() {
        let queue = make_queue_with_lanes().await;
        let config = MonitorConfig {
            interval: Duration::from_millis(30),
            pending_warning_threshold: 100,
            active_warning_threshold: 50,
        };
        let monitor = Arc::new(QueueMonitor::with_config(queue, config));

        // Clone Arc before start() consumes it
        let monitor_ref = Arc::clone(&monitor);
        monitor.start().await;

        // Let monitor run a few cycles
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify the monitor is still functional via the cloned reference
        let stats = monitor_ref.stats().await;
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
    }

    #[tokio::test]
    async fn test_monitor_start_with_commands() {
        let queue = make_queue_with_lanes().await;

        // Add some pending commands
        for _ in 0..3 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = queue.submit(lane_ids::QUERY, cmd).await;
        }

        let config = MonitorConfig {
            interval: Duration::from_millis(20),
            pending_warning_threshold: 2, // Below current pending, will trigger warning
            active_warning_threshold: 50,
        };
        let monitor = Arc::new(QueueMonitor::with_config(queue, config));

        // Clone Arc before start() consumes it
        let monitor_ref = Arc::clone(&monitor);
        monitor.start().await;

        // Let monitor run and check health (should not panic even with warnings)
        tokio::time::sleep(Duration::from_millis(80)).await;

        let stats = monitor_ref.stats().await;
        assert_eq!(stats.total_pending, 3);
    }

    // ========================================================================
    // QueueStats Tests
    // ========================================================================

    #[test]
    fn test_queue_stats_default() {
        let stats = QueueStats::default();
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
        assert!(stats.lanes.is_empty());
    }

    #[test]
    fn test_queue_stats_serialization() {
        let mut lanes = std::collections::HashMap::new();
        lanes.insert(
            "query".to_string(),
            crate::queue::LaneStatus {
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
        assert_eq!(parsed.lanes["query"].pending, 5);
        assert_eq!(parsed.lanes["query"].max, 10);
    }

    #[test]
    fn test_queue_stats_clone() {
        let stats = QueueStats {
            total_pending: 10,
            total_active: 3,
            dead_letter_count: 0,
            lanes: std::collections::HashMap::new(),
        };
        let cloned = stats.clone();
        assert_eq!(cloned.total_pending, 10);
        assert_eq!(cloned.total_active, 3);
    }
}
