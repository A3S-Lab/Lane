//! Queue partitioning for parallel processing
//!
//! This module provides partitioning support for distributing commands across
//! multiple workers, enabling efficient multi-core and distributed processing.

use crate::queue::Command;
use async_trait::async_trait;
use std::sync::Arc;

/// Partition identifier
pub type PartitionId = usize;

/// Partitioner trait for distributing commands across partitions
#[async_trait]
pub trait Partitioner: Send + Sync {
    /// Determine which partition a command should be assigned to
    ///
    /// Returns a partition ID in the range [0, num_partitions)
    async fn partition(&self, command: &dyn Command, num_partitions: usize) -> PartitionId;
}

/// Round-robin partitioner - distributes commands evenly across partitions
pub struct RoundRobinPartitioner {
    counter: std::sync::atomic::AtomicUsize,
}

impl RoundRobinPartitioner {
    pub fn new() -> Self {
        Self {
            counter: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl Default for RoundRobinPartitioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Partitioner for RoundRobinPartitioner {
    async fn partition(&self, _command: &dyn Command, num_partitions: usize) -> PartitionId {
        let count = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        count % num_partitions
    }
}

/// Hash-based partitioner - assigns commands to partitions based on command type hash
pub struct HashPartitioner;

impl HashPartitioner {
    pub fn new() -> Self {
        Self
    }

    fn hash_string(s: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

impl Default for HashPartitioner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Partitioner for HashPartitioner {
    async fn partition(&self, command: &dyn Command, num_partitions: usize) -> PartitionId {
        let hash = Self::hash_string(command.command_type());
        (hash as usize) % num_partitions
    }
}

/// Custom partitioner that uses a user-provided function
pub struct CustomPartitioner<F>
where
    F: Fn(&str) -> usize + Send + Sync,
{
    func: F,
}

impl<F> CustomPartitioner<F>
where
    F: Fn(&str) -> usize + Send + Sync,
{
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

#[async_trait]
impl<F> Partitioner for CustomPartitioner<F>
where
    F: Fn(&str) -> usize + Send + Sync,
{
    async fn partition(&self, command: &dyn Command, num_partitions: usize) -> PartitionId {
        (self.func)(command.command_type()) % num_partitions
    }
}

/// Partition configuration
#[derive(Debug, Clone)]
pub struct PartitionConfig {
    /// Number of partitions (typically set to number of CPU cores)
    pub num_partitions: usize,
    /// Partitioner strategy
    pub strategy: PartitionStrategy,
}

/// Partitioning strategy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStrategy {
    /// Round-robin distribution
    RoundRobin,
    /// Hash-based distribution (same command types go to same partition)
    Hash,
    /// No partitioning (single partition)
    None,
}

impl PartitionConfig {
    /// Create a new partition configuration
    pub fn new(num_partitions: usize, strategy: PartitionStrategy) -> Self {
        Self {
            num_partitions,
            strategy,
        }
    }

    /// Create a partition config using all available CPU cores
    pub fn auto() -> Self {
        let num_cores = num_cpus::get();
        Self {
            num_partitions: num_cores,
            strategy: PartitionStrategy::RoundRobin,
        }
    }

    /// Create a partition config with hash-based strategy
    pub fn hash(num_partitions: usize) -> Self {
        Self {
            num_partitions,
            strategy: PartitionStrategy::Hash,
        }
    }

    /// Create a partition config with round-robin strategy
    pub fn round_robin(num_partitions: usize) -> Self {
        Self {
            num_partitions,
            strategy: PartitionStrategy::RoundRobin,
        }
    }

    /// No partitioning (single partition)
    pub fn none() -> Self {
        Self {
            num_partitions: 1,
            strategy: PartitionStrategy::None,
        }
    }

    /// Create a partitioner instance based on the strategy
    pub fn create_partitioner(&self) -> Arc<dyn Partitioner> {
        match self.strategy {
            PartitionStrategy::RoundRobin => Arc::new(RoundRobinPartitioner::new()),
            PartitionStrategy::Hash => Arc::new(HashPartitioner::new()),
            PartitionStrategy::None => Arc::new(RoundRobinPartitioner::new()),
        }
    }
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self::auto()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;

    struct TestCommand {
        cmd_type: String,
    }

    #[async_trait::async_trait]
    impl Command for TestCommand {
        async fn execute(&self) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }

        fn command_type(&self) -> &str {
            &self.cmd_type
        }
    }

    #[tokio::test]
    async fn test_round_robin_partitioner() {
        let partitioner = RoundRobinPartitioner::new();
        let cmd = TestCommand {
            cmd_type: "test".to_string(),
        };

        let p1 = partitioner.partition(&cmd, 4).await;
        let p2 = partitioner.partition(&cmd, 4).await;
        let p3 = partitioner.partition(&cmd, 4).await;
        let p4 = partitioner.partition(&cmd, 4).await;
        let p5 = partitioner.partition(&cmd, 4).await;

        assert_eq!(p1, 0);
        assert_eq!(p2, 1);
        assert_eq!(p3, 2);
        assert_eq!(p4, 3);
        assert_eq!(p5, 0); // Wraps around
    }

    #[tokio::test]
    async fn test_hash_partitioner() {
        let partitioner = HashPartitioner::new();
        let cmd1 = TestCommand {
            cmd_type: "type_a".to_string(),
        };
        let cmd2 = TestCommand {
            cmd_type: "type_b".to_string(),
        };

        // Same command type should always go to same partition
        let p1 = partitioner.partition(&cmd1, 4).await;
        let p2 = partitioner.partition(&cmd1, 4).await;
        assert_eq!(p1, p2);

        // Different command types may go to different partitions
        let p3 = partitioner.partition(&cmd2, 4).await;
        // We can't assert they're different, but they should be consistent
        let p4 = partitioner.partition(&cmd2, 4).await;
        assert_eq!(p3, p4);
    }

    #[tokio::test]
    async fn test_custom_partitioner() {
        let partitioner = CustomPartitioner::new(|cmd_type: &str| {
            if cmd_type.starts_with("high") {
                0
            } else {
                1
            }
        });

        let cmd_high = TestCommand {
            cmd_type: "high_priority".to_string(),
        };
        let cmd_low = TestCommand {
            cmd_type: "low_priority".to_string(),
        };

        let p1 = partitioner.partition(&cmd_high, 4).await;
        let p2 = partitioner.partition(&cmd_low, 4).await;

        assert_eq!(p1, 0);
        assert_eq!(p2, 1);
    }

    #[test]
    fn test_partition_config_auto() {
        let config = PartitionConfig::auto();
        assert!(config.num_partitions > 0);
        assert_eq!(config.strategy, PartitionStrategy::RoundRobin);
    }

    #[test]
    fn test_partition_config_hash() {
        let config = PartitionConfig::hash(8);
        assert_eq!(config.num_partitions, 8);
        assert_eq!(config.strategy, PartitionStrategy::Hash);
    }

    #[test]
    fn test_partition_config_none() {
        let config = PartitionConfig::none();
        assert_eq!(config.num_partitions, 1);
        assert_eq!(config.strategy, PartitionStrategy::None);
    }
}
