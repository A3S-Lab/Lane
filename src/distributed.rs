//! Distributed queue support for multi-machine parallel processing
//!
//! This module provides traits and implementations for distributed queue processing.
//! The default implementation uses local multi-core parallelism, but users can
//! implement the `DistributedQueue` trait for multi-machine distributed processing.

use crate::error::Result;
use crate::partition::{PartitionConfig, PartitionId, Partitioner};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Worker identifier
pub type WorkerId = String;

/// Distributed command envelope for serialization across workers
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    /// Unique command ID
    pub id: String,
    /// Command type identifier
    pub command_type: String,
    /// Lane ID
    pub lane_id: String,
    /// Partition ID
    pub partition_id: PartitionId,
    /// Serialized command payload
    pub payload: serde_json::Value,
    /// Retry count
    pub retry_count: u32,
    /// Created timestamp
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Result of command execution from a worker
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// Command ID
    pub command_id: String,
    /// Success or error
    pub result: std::result::Result<serde_json::Value, String>,
    /// Worker that executed the command
    pub worker_id: WorkerId,
    /// Execution duration in milliseconds
    pub duration_ms: u64,
}

/// Distributed queue trait for multi-machine parallel processing
///
/// Implement this trait to enable distributed queue processing across multiple machines.
/// The default implementation (`LocalDistributedQueue`) uses local multi-core parallelism.
#[async_trait]
pub trait DistributedQueue: Send + Sync {
    /// Enqueue a command to be processed by a worker
    async fn enqueue(&self, envelope: CommandEnvelope) -> Result<()>;

    /// Dequeue a command for processing (called by workers)
    async fn dequeue(&self, partition_id: PartitionId) -> Result<Option<CommandEnvelope>>;

    /// Report command completion
    async fn complete(&self, result: CommandResult) -> Result<()>;

    /// Get the number of partitions
    fn num_partitions(&self) -> usize;

    /// Get the worker ID for this instance
    fn worker_id(&self) -> &WorkerId;

    /// Check if this instance is a coordinator (can enqueue commands)
    fn is_coordinator(&self) -> bool;

    /// Check if this instance is a worker (can dequeue and execute commands)
    fn is_worker(&self) -> bool;
}

/// Local distributed queue implementation using multi-core parallelism
///
/// This is the default implementation that uses local channels for communication
/// between partitions. Each partition runs on a separate tokio task, enabling
/// efficient multi-core utilization.
pub struct LocalDistributedQueue {
    worker_id: WorkerId,
    partition_config: PartitionConfig,
    partitioner: Arc<dyn Partitioner>,
    /// Channels for each partition (sender side)
    partition_senders: Vec<mpsc::Sender<CommandEnvelope>>,
    /// Channels for each partition (receiver side, wrapped in mutex for sharing)
    partition_receivers: Vec<Arc<tokio::sync::Mutex<mpsc::Receiver<CommandEnvelope>>>>,
    /// Channel for completed results
    result_sender: mpsc::Sender<CommandResult>,
    result_receiver: Arc<tokio::sync::Mutex<mpsc::Receiver<CommandResult>>>,
}

impl LocalDistributedQueue {
    /// Create a new local distributed queue with the specified partition configuration
    pub fn new(partition_config: PartitionConfig) -> Self {
        let num_partitions = partition_config.num_partitions;
        let partitioner = partition_config.create_partitioner();

        let mut partition_senders = Vec::with_capacity(num_partitions);
        let mut partition_receivers = Vec::with_capacity(num_partitions);

        // Create channels for each partition
        for _ in 0..num_partitions {
            let (tx, rx) = mpsc::channel(1000); // Buffer size per partition
            partition_senders.push(tx);
            partition_receivers.push(Arc::new(tokio::sync::Mutex::new(rx)));
        }

        // Create result channel
        let (result_tx, result_rx) = mpsc::channel(1000);

        Self {
            worker_id: format!("local-{}", uuid::Uuid::new_v4()),
            partition_config,
            partitioner,
            partition_senders,
            partition_receivers,
            result_sender: result_tx,
            result_receiver: Arc::new(tokio::sync::Mutex::new(result_rx)),
        }
    }

    /// Create a local distributed queue that automatically uses all CPU cores
    pub fn auto() -> Self {
        Self::new(PartitionConfig::auto())
    }

    /// Get the partitioner
    pub fn partitioner(&self) -> &Arc<dyn Partitioner> {
        &self.partitioner
    }

    /// Get the partition configuration
    pub fn partition_config(&self) -> &PartitionConfig {
        &self.partition_config
    }

    /// Get the receiver for a specific partition (for worker tasks)
    pub fn partition_receiver(
        &self,
        partition_id: PartitionId,
    ) -> Option<Arc<tokio::sync::Mutex<mpsc::Receiver<CommandEnvelope>>>> {
        self.partition_receivers.get(partition_id).cloned()
    }

    /// Get the result receiver (for coordinator to collect results)
    pub fn result_receiver(&self) -> Arc<tokio::sync::Mutex<mpsc::Receiver<CommandResult>>> {
        Arc::clone(&self.result_receiver)
    }

    /// Get the result sender (for workers to send results)
    pub fn result_sender(&self) -> mpsc::Sender<CommandResult> {
        self.result_sender.clone()
    }
}

#[async_trait]
impl DistributedQueue for LocalDistributedQueue {
    async fn enqueue(&self, envelope: CommandEnvelope) -> Result<()> {
        let partition_id = envelope.partition_id;
        if partition_id >= self.partition_senders.len() {
            return Err(crate::error::LaneError::Other(format!(
                "Invalid partition ID: {}",
                partition_id
            )));
        }

        self.partition_senders[partition_id]
            .send(envelope)
            .await
            .map_err(|e| crate::error::LaneError::Other(format!("Failed to enqueue: {}", e)))?;

        Ok(())
    }

    async fn dequeue(&self, partition_id: PartitionId) -> Result<Option<CommandEnvelope>> {
        if partition_id >= self.partition_receivers.len() {
            return Err(crate::error::LaneError::Other(format!(
                "Invalid partition ID: {}",
                partition_id
            )));
        }

        let mut receiver = self.partition_receivers[partition_id].lock().await;
        match receiver.try_recv() {
            Ok(envelope) => Ok(Some(envelope)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Ok(None),
        }
    }

    async fn complete(&self, result: CommandResult) -> Result<()> {
        self.result_sender
            .send(result)
            .await
            .map_err(|e| crate::error::LaneError::Other(format!("Failed to send result: {}", e)))?;
        Ok(())
    }

    fn num_partitions(&self) -> usize {
        self.partition_config.num_partitions
    }

    fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    fn is_coordinator(&self) -> bool {
        true // Local queue is always both coordinator and worker
    }

    fn is_worker(&self) -> bool {
        true // Local queue is always both coordinator and worker
    }
}

/// Worker pool for processing commands across multiple partitions
pub struct WorkerPool {
    distributed_queue: Arc<dyn DistributedQueue>,
    worker_handles: Vec<tokio::task::JoinHandle<()>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerPool {
    /// Create a new worker pool with the given distributed queue
    pub fn new(distributed_queue: Arc<dyn DistributedQueue>) -> Self {
        Self {
            distributed_queue,
            worker_handles: Vec::new(),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Start worker tasks for all partitions
    ///
    /// The `command_executor` function is called for each command to execute it.
    pub fn start<F, Fut>(&mut self, command_executor: F)
    where
        F: Fn(CommandEnvelope) -> Fut + Send + Sync + Clone + 'static,
        Fut: std::future::Future<Output = std::result::Result<serde_json::Value, String>>
            + Send
            + 'static,
    {
        let num_partitions = self.distributed_queue.num_partitions();

        for partition_id in 0..num_partitions {
            let queue = Arc::clone(&self.distributed_queue);
            let shutdown = Arc::clone(&self.shutdown);
            let executor = command_executor.clone();

            let handle = tokio::spawn(async move {
                loop {
                    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }

                    match queue.dequeue(partition_id).await {
                        Ok(Some(envelope)) => {
                            let command_id = envelope.id.clone();
                            let start = std::time::Instant::now();

                            let result = executor(envelope).await;
                            let duration_ms = start.elapsed().as_millis() as u64;

                            let command_result = CommandResult {
                                command_id,
                                result,
                                worker_id: queue.worker_id().clone(),
                                duration_ms,
                            };

                            let _ = queue.complete(command_result).await;
                        }
                        Ok(None) => {
                            // No command available, sleep briefly
                            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                        }
                        Err(_) => {
                            // Error dequeuing, sleep and retry
                            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
                        }
                    }
                }
            });

            self.worker_handles.push(handle);
        }
    }

    /// Shutdown all workers
    pub async fn shutdown(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);

        for handle in self.worker_handles.drain(..) {
            let _ = handle.await;
        }
    }

    /// Check if workers are running
    pub fn is_running(&self) -> bool {
        !self.worker_handles.is_empty()
            && !self.shutdown.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_distributed_queue_creation() {
        let queue = LocalDistributedQueue::auto();
        assert!(queue.num_partitions() > 0);
        assert!(queue.is_coordinator());
        assert!(queue.is_worker());
    }

    #[tokio::test]
    async fn test_local_distributed_queue_enqueue_dequeue() {
        let queue = LocalDistributedQueue::new(PartitionConfig::new(
            2,
            crate::partition::PartitionStrategy::RoundRobin,
        ));

        let envelope = CommandEnvelope {
            id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            partition_id: 0,
            payload: serde_json::json!({"data": "test"}),
            retry_count: 0,
            created_at: chrono::Utc::now(),
        };

        // Enqueue
        queue.enqueue(envelope.clone()).await.unwrap();

        // Dequeue
        let dequeued = queue.dequeue(0).await.unwrap();
        assert!(dequeued.is_some());
        let dequeued = dequeued.unwrap();
        assert_eq!(dequeued.id, "cmd1");
        assert_eq!(dequeued.command_type, "test");

        // Dequeue again should be empty
        let dequeued = queue.dequeue(0).await.unwrap();
        assert!(dequeued.is_none());
    }

    #[tokio::test]
    async fn test_local_distributed_queue_complete() {
        let queue = LocalDistributedQueue::new(PartitionConfig::new(
            2,
            crate::partition::PartitionStrategy::RoundRobin,
        ));

        let result = CommandResult {
            command_id: "cmd1".to_string(),
            result: Ok(serde_json::json!({"success": true})),
            worker_id: "worker1".to_string(),
            duration_ms: 100,
        };

        queue.complete(result).await.unwrap();

        // Check result was received
        let receiver_arc = queue.result_receiver();
        let mut receiver = receiver_arc.lock().await;
        let received = receiver.try_recv();
        assert!(received.is_ok());
        let received = received.unwrap();
        assert_eq!(received.command_id, "cmd1");
    }

    #[tokio::test]
    async fn test_worker_pool() {
        let queue = Arc::new(LocalDistributedQueue::new(PartitionConfig::new(
            2,
            crate::partition::PartitionStrategy::RoundRobin,
        )));

        let mut pool = WorkerPool::new(queue.clone());

        // Start workers with a simple executor
        pool.start(|envelope| async move {
            Ok(serde_json::json!({"processed": envelope.id}))
        });

        assert!(pool.is_running());

        // Enqueue a command
        let envelope = CommandEnvelope {
            id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            partition_id: 0,
            payload: serde_json::json!({}),
            retry_count: 0,
            created_at: chrono::Utc::now(),
        };
        queue.enqueue(envelope).await.unwrap();

        // Wait for processing
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Check result
        let receiver_arc = queue.result_receiver();
        let mut receiver = receiver_arc.lock().await;
        let result = receiver.try_recv();
        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.command_id, "cmd1");
        assert!(result.result.is_ok());

        // Shutdown
        pool.shutdown().await;
        assert!(!pool.is_running());
    }

    #[test]
    fn test_command_envelope_serialization() {
        let envelope = CommandEnvelope {
            id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            partition_id: 0,
            payload: serde_json::json!({"key": "value"}),
            retry_count: 2,
            created_at: chrono::Utc::now(),
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: CommandEnvelope = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, "cmd1");
        assert_eq!(parsed.command_type, "test");
        assert_eq!(parsed.partition_id, 0);
        assert_eq!(parsed.retry_count, 2);
    }

    #[test]
    fn test_command_result_serialization() {
        let result = CommandResult {
            command_id: "cmd1".to_string(),
            result: Ok(serde_json::json!({"success": true})),
            worker_id: "worker1".to_string(),
            duration_ms: 150,
        };

        let json = serde_json::to_string(&result).unwrap();
        let parsed: CommandResult = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.command_id, "cmd1");
        assert_eq!(parsed.worker_id, "worker1");
        assert_eq!(parsed.duration_ms, 150);
    }
}
