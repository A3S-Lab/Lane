//! Queue manager provides high-level queue management

use crate::alerts::AlertManager;
use crate::config::LaneConfig;
use crate::error::Result;
use crate::event::EventEmitter;
use crate::metrics::QueueMetrics;
use crate::queue::{lane_ids, priorities, Command, CommandQueue, Lane};
use crate::storage::Storage;
use crate::QueueStats;
use std::collections::HashMap;
use std::sync::Arc;

/// Queue manager
#[allow(dead_code)]
pub struct QueueManager {
    queue: Arc<CommandQueue>,
    scheduler_handle: tokio::sync::Mutex<Option<()>>,
    metrics: Option<QueueMetrics>,
    alerts: Option<Arc<AlertManager>>,
}

impl QueueManager {
    /// Create a new queue manager
    #[allow(dead_code)]
    pub(crate) fn new(queue: Arc<CommandQueue>) -> Self {
        Self {
            queue,
            scheduler_handle: tokio::sync::Mutex::new(None),
            metrics: None,
            alerts: None,
        }
    }

    /// Create a new queue manager with metrics and alerts
    pub(crate) fn with_observability(
        queue: Arc<CommandQueue>,
        metrics: Option<QueueMetrics>,
        alerts: Option<Arc<AlertManager>>,
    ) -> Self {
        Self {
            queue,
            scheduler_handle: tokio::sync::Mutex::new(None),
            metrics,
            alerts,
        }
    }

    /// Start the queue scheduler
    pub async fn start(&self) -> anyhow::Result<()> {
        tracing::info!("Starting queue scheduler");
        let queue = Arc::clone(&self.queue);
        queue.start_scheduler().await;
        Ok(())
    }

    /// Submit a command to a lane
    pub async fn submit(
        &self,
        lane_id: &str,
        command: Box<dyn Command>,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<serde_json::Value>>> {
        self.queue.submit(lane_id, command).await
    }

    /// Get queue statistics
    pub async fn stats(&self) -> anyhow::Result<QueueStats> {
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

        Ok(QueueStats {
            total_pending,
            total_active,
            dead_letter_count,
            lanes: lane_status,
        })
    }

    /// Get the underlying command queue
    pub fn queue(&self) -> Arc<CommandQueue> {
        Arc::clone(&self.queue)
    }

    /// Initiate graceful shutdown - stop accepting new commands
    pub async fn shutdown(&self) {
        self.queue.shutdown().await;
    }

    /// Wait for all pending commands to complete (with timeout)
    pub async fn drain(&self, timeout: std::time::Duration) -> Result<()> {
        self.queue.drain(timeout).await
    }

    /// Check if shutdown is in progress
    pub fn is_shutting_down(&self) -> bool {
        self.queue.is_shutting_down()
    }

    /// Get the metrics collector (if configured)
    pub fn metrics(&self) -> Option<&QueueMetrics> {
        self.metrics.as_ref()
    }

    /// Get the alert manager (if configured)
    pub fn alerts(&self) -> Option<&Arc<AlertManager>> {
        self.alerts.as_ref()
    }
}

/// Queue manager builder provides a high-level API for managing the command queue
pub struct QueueManagerBuilder {
    event_emitter: EventEmitter,
    lane_configs: HashMap<String, (LaneConfig, u8)>,
    storage: Option<Arc<dyn Storage>>,
    dlq_size: Option<usize>,
    metrics: Option<QueueMetrics>,
    alerts: Option<Arc<AlertManager>>,
}

impl QueueManagerBuilder {
    /// Create a new queue manager builder
    pub fn new(event_emitter: EventEmitter) -> Self {
        Self {
            event_emitter,
            lane_configs: HashMap::new(),
            storage: None,
            dlq_size: None,
            metrics: None,
            alerts: None,
        }
    }

    /// Add a lane configuration
    pub fn with_lane(mut self, id: impl Into<String>, config: LaneConfig, priority: u8) -> Self {
        self.lane_configs.insert(id.into(), (config, priority));
        self
    }

    /// Add storage backend for persistence
    pub fn with_storage(mut self, storage: Arc<dyn Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Add dead letter queue with specified size
    pub fn with_dlq(mut self, size: usize) -> Self {
        self.dlq_size = Some(size);
        self
    }

    /// Add metrics collection
    pub fn with_metrics(mut self, metrics: QueueMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Add alert manager
    pub fn with_alerts(mut self, alerts: Arc<AlertManager>) -> Self {
        self.alerts = Some(alerts);
        self
    }

    /// Add default lanes (system, control, query, session, skill, prompt)
    pub fn with_default_lanes(mut self) -> Self {
        self.lane_configs.insert(
            lane_ids::SYSTEM.to_string(),
            (LaneConfig::new(1, 5), priorities::SYSTEM),
        );
        self.lane_configs.insert(
            lane_ids::CONTROL.to_string(),
            (LaneConfig::new(1, 3), priorities::CONTROL),
        );
        self.lane_configs.insert(
            lane_ids::QUERY.to_string(),
            (LaneConfig::new(1, 10), priorities::QUERY),
        );
        self.lane_configs.insert(
            lane_ids::SESSION.to_string(),
            (LaneConfig::new(1, 5), priorities::SESSION),
        );
        self.lane_configs.insert(
            lane_ids::SKILL.to_string(),
            (LaneConfig::new(1, 3), priorities::SKILL),
        );
        self.lane_configs.insert(
            lane_ids::PROMPT.to_string(),
            (LaneConfig::new(1, 2), priorities::PROMPT),
        );
        self
    }

    /// Build the queue manager
    pub async fn build(self) -> anyhow::Result<QueueManager> {
        // Create queue with appropriate configuration
        let queue = match (self.dlq_size, self.storage.clone()) {
            (Some(dlq_size), Some(storage)) => Arc::new(CommandQueue::with_dlq_and_storage(
                self.event_emitter,
                dlq_size,
                storage.clone(),
            )),
            (Some(dlq_size), None) => {
                Arc::new(CommandQueue::with_dlq(self.event_emitter, dlq_size))
            }
            (None, Some(storage)) => Arc::new(CommandQueue::with_storage(
                self.event_emitter,
                storage.clone(),
            )),
            (None, None) => Arc::new(CommandQueue::new(self.event_emitter)),
        };

        // Register all lanes
        for (id, (config, priority)) in self.lane_configs {
            let lane = if let Some(storage) = &self.storage {
                Arc::new(Lane::with_storage(id, config, priority, storage.clone()))
            } else {
                Arc::new(Lane::new(id, config, priority))
            };
            queue.register_lane(lane).await;
        }

        Ok(QueueManager::with_observability(
            queue,
            self.metrics,
            self.alerts,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LaneError;
    use async_trait::async_trait;

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

    struct FailingCommand {
        message: String,
    }

    #[async_trait]
    impl Command for FailingCommand {
        async fn execute(&self) -> Result<serde_json::Value> {
            Err(LaneError::Other(self.message.clone()))
        }
        fn command_type(&self) -> &str {
            "failing"
        }
    }

    /// Helper: build a QueueManager with standard lanes
    async fn make_manager() -> QueueManager {
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

        QueueManager::new(queue)
    }

    // ========================================================================
    // Builder Tests
    // ========================================================================

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

    #[tokio::test]
    async fn test_queue_manager_builder_custom_lanes() {
        let emitter = EventEmitter::new(100);
        let manager = QueueManagerBuilder::new(emitter)
            .with_lane("custom1", LaneConfig::new(1, 4), 0)
            .with_lane("custom2", LaneConfig::new(2, 8), 1)
            .build()
            .await
            .unwrap();

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.lanes.len(), 2);
        assert!(stats.lanes.contains_key("custom1"));
        assert!(stats.lanes.contains_key("custom2"));
    }

    // ========================================================================
    // Construction Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_new() {
        let manager = make_manager().await;

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
        assert_eq!(stats.lanes.len(), 4);
    }

    #[tokio::test]
    async fn test_manager_queue_accessor() {
        let manager = make_manager().await;

        let queue = manager.queue();
        let status = queue.status().await;
        assert_eq!(status.len(), 4);
    }

    // ========================================================================
    // stats() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_stats_empty() {
        let manager = make_manager().await;

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);

        // Check each lane
        assert_eq!(stats.lanes[lane_ids::SYSTEM].pending, 0);
        assert_eq!(stats.lanes[lane_ids::SYSTEM].max, 5);
        assert_eq!(stats.lanes[lane_ids::CONTROL].max, 3);
        assert_eq!(stats.lanes[lane_ids::QUERY].max, 10);
        assert_eq!(stats.lanes[lane_ids::PROMPT].max, 2);
    }

    #[tokio::test]
    async fn test_manager_stats_with_pending() {
        let manager = make_manager().await;

        // Submit commands without starting scheduler
        for _ in 0..4 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = manager.submit(lane_ids::QUERY, cmd).await;
        }

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 4);
        assert_eq!(stats.total_active, 0);
        assert_eq!(stats.lanes[lane_ids::QUERY].pending, 4);
    }

    #[tokio::test]
    async fn test_manager_stats_multiple_lanes() {
        let manager = make_manager().await;

        let _ = manager
            .submit(
                lane_ids::SYSTEM,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;
        let _ = manager
            .submit(
                lane_ids::QUERY,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;
        let _ = manager
            .submit(
                lane_ids::QUERY,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;
        let _ = manager
            .submit(
                lane_ids::PROMPT,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 4);
        assert_eq!(stats.lanes[lane_ids::SYSTEM].pending, 1);
        assert_eq!(stats.lanes[lane_ids::QUERY].pending, 2);
        assert_eq!(stats.lanes[lane_ids::PROMPT].pending, 1);
        assert_eq!(stats.lanes[lane_ids::CONTROL].pending, 0);
    }

    // ========================================================================
    // submit() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_submit_valid_lane() {
        let manager = make_manager().await;

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"data": "ok"}),
        });
        let result = manager.submit(lane_ids::QUERY, cmd).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_manager_submit_unknown_lane() {
        let manager = make_manager().await;

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({}),
        });
        let result = manager.submit("nonexistent-lane", cmd).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_manager_submit_and_execute() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"key": "value"}),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"key": "value"}));
    }

    #[tokio::test]
    async fn test_manager_submit_failing_command() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        let cmd = Box::new(FailingCommand {
            message: "manager test failure".to_string(),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_manager_submit_multiple_commands() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        let mut receivers = Vec::new();
        for i in 0..5 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({"index": i}),
            });
            let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();
            receivers.push(rx);
        }

        for (i, rx) in receivers.into_iter().enumerate() {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
                .await
                .expect("Timeout")
                .expect("Channel closed");
            assert!(result.is_ok());
            let val = result.unwrap();
            assert_eq!(val["index"], i);
        }
    }

    // ========================================================================
    // start() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_start() {
        let manager = make_manager().await;

        let result = manager.start().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_manager_start_drains_pending() {
        let manager = make_manager().await;

        // Submit before start
        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"queued": true}),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        // Verify pending
        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 1);

        // Start scheduler
        manager.start().await.unwrap();

        // Command should now execute
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["queued"], true);
    }

    // ========================================================================
    // queue() Accessor Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_queue_returns_same_instance() {
        let manager = make_manager().await;

        let q1 = manager.queue();
        let q2 = manager.queue();

        // Both should point to the same underlying data
        assert!(Arc::ptr_eq(&q1, &q2));
    }

    #[tokio::test]
    async fn test_manager_queue_can_submit_directly() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        // Submit directly via underlying queue
        let queue = manager.queue();
        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"direct": true}),
        });
        let rx = queue.submit(lane_ids::SYSTEM, cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["direct"], true);
    }

    // ========================================================================
    // Shutdown Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_shutdown() {
        let manager = make_manager().await;

        assert!(!manager.is_shutting_down());

        manager.shutdown().await;
        assert!(manager.is_shutting_down());
    }

    #[tokio::test]
    async fn test_manager_shutdown_rejects_commands() {
        let manager = make_manager().await;

        manager.shutdown().await;

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({}),
        });
        let result = manager.submit(lane_ids::QUERY, cmd).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_manager_drain() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        // Submit a command
        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"test": "data"}),
        });
        let _rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        // Shutdown and drain
        manager.shutdown().await;
        let drain_result = manager.drain(std::time::Duration::from_secs(2)).await;

        assert!(drain_result.is_ok());
    }

    // ========================================================================
    // Storage Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_with_storage() {
        use crate::storage::LocalStorage;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(
            LocalStorage::new(temp_dir.path().to_path_buf())
                .await
                .unwrap(),
        );

        let emitter = EventEmitter::new(100);
        let manager = QueueManagerBuilder::new(emitter)
            .with_storage(storage.clone())
            .with_lane(lane_ids::QUERY, LaneConfig::new(1, 10), priorities::QUERY)
            .build()
            .await
            .unwrap();

        manager.start().await.unwrap();

        // Submit a command
        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"stored": true}),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        // Wait for command to complete
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());

        // Verify command was removed from storage after completion
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stored_commands = storage.load_commands().await.unwrap();
        assert_eq!(stored_commands.len(), 0);
    }

    #[tokio::test]
    async fn test_manager_with_storage_and_dlq() {
        use crate::storage::LocalStorage;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(
            LocalStorage::new(temp_dir.path().to_path_buf())
                .await
                .unwrap(),
        );

        let emitter = EventEmitter::new(100);
        let manager = QueueManagerBuilder::new(emitter)
            .with_storage(storage.clone())
            .with_dlq(100)
            .with_lane(lane_ids::QUERY, LaneConfig::new(1, 10), priorities::QUERY)
            .build()
            .await
            .unwrap();

        manager.start().await.unwrap();

        // Submit a failing command
        let cmd = Box::new(FailingCommand {
            message: "test error".to_string(),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        // Wait for command to fail
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_err());

        // Verify command was removed from storage after failure
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let stored_commands = storage.load_commands().await.unwrap();
        assert_eq!(stored_commands.len(), 0);

        // Verify DLQ has the failed command
        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.dead_letter_count, 1);
    }

    // ========================================================================
    // Observability Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_with_metrics() {
        let emitter = EventEmitter::new(100);
        let metrics = QueueMetrics::local();

        let manager = QueueManagerBuilder::new(emitter)
            .with_metrics(metrics.clone())
            .with_lane(lane_ids::QUERY, LaneConfig::new(1, 10), priorities::QUERY)
            .build()
            .await
            .unwrap();

        assert!(manager.metrics().is_some());

        // Verify metrics are accessible
        let mgr_metrics = manager.metrics().unwrap();
        let snapshot = mgr_metrics.snapshot().await;
        assert!(snapshot.counters.is_empty());
    }

    #[tokio::test]
    async fn test_manager_with_alerts() {
        let emitter = EventEmitter::new(100);
        let alerts = Arc::new(AlertManager::with_queue_depth_alerts(100, 200));

        let manager = QueueManagerBuilder::new(emitter)
            .with_alerts(alerts.clone())
            .with_lane(lane_ids::QUERY, LaneConfig::new(1, 10), priorities::QUERY)
            .build()
            .await
            .unwrap();

        assert!(manager.alerts().is_some());

        // Verify alerts are accessible
        let mgr_alerts = manager.alerts().unwrap();
        let config = mgr_alerts.queue_depth_config().await;
        assert_eq!(config.warning_threshold, 100);
        assert_eq!(config.critical_threshold, 200);
    }

    #[tokio::test]
    async fn test_manager_with_metrics_and_alerts() {
        let emitter = EventEmitter::new(100);
        let metrics = QueueMetrics::local();
        let alerts = Arc::new(AlertManager::with_latency_alerts(100.0, 500.0));

        let manager = QueueManagerBuilder::new(emitter)
            .with_metrics(metrics)
            .with_alerts(alerts)
            .with_lane(lane_ids::QUERY, LaneConfig::new(1, 10), priorities::QUERY)
            .build()
            .await
            .unwrap();

        assert!(manager.metrics().is_some());
        assert!(manager.alerts().is_some());
    }

    #[tokio::test]
    async fn test_manager_without_observability() {
        let emitter = EventEmitter::new(100);
        let manager = QueueManagerBuilder::new(emitter)
            .with_lane(lane_ids::QUERY, LaneConfig::new(1, 10), priorities::QUERY)
            .build()
            .await
            .unwrap();

        assert!(manager.metrics().is_none());
        assert!(manager.alerts().is_none());
    }
}
