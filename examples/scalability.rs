//! Scalability features example
//!
//! This example demonstrates:
//! - Rate limiting (token bucket and sliding window)
//! - Priority boosting
//! - Queue partitioning
//! - Multi-core parallelism

use a3s_lane::{
    Command, DistributedQueue, EventEmitter, LaneConfig, LocalDistributedQueue, PartitionConfig,
    PriorityBoostConfig, QueueManagerBuilder, RateLimitConfig, Result,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// A simple work command
struct WorkCommand {
    id: String,
}

#[async_trait]
impl Command for WorkCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(serde_json::json!({
            "id": self.id,
            "timestamp": chrono::Utc::now().to_rfc3339()
        }))
    }

    fn command_type(&self) -> &str {
        "work"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    println!("=== A3S Lane: Scalability Features Example ===\n");

    // Example 1: Rate Limiting
    println!("=== Example 1: Rate Limiting ===");

    // Token bucket: 5 commands per second
    let rate_limit = RateLimitConfig::per_second(5);
    println!(
        "Rate limit: {} commands per second",
        rate_limit.max_commands
    );

    let config = LaneConfig::new(1, 10).with_rate_limit(rate_limit);

    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_lane("rate-limited", config, 0)
        .build()
        .await?;

    manager.start().await?;

    // Submit 10 commands quickly
    println!("Submitting 10 commands...");
    let start = std::time::Instant::now();
    let mut receivers = Vec::new();

    for i in 0..10 {
        let cmd = Box::new(WorkCommand {
            id: format!("rl-{}", i),
        });
        let rx = manager.submit("rate-limited", cmd).await?;
        receivers.push(rx);
    }

    // Wait for all to complete
    for rx in receivers {
        let _ = rx.await;
    }

    let elapsed = start.elapsed();
    println!(
        "✓ Completed 10 commands in {:.2}s (rate limited)",
        elapsed.as_secs_f64()
    );

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    println!();

    // Example 2: Priority Boosting
    println!("=== Example 2: Priority Boosting ===");

    let boost_config = PriorityBoostConfig::standard(Duration::from_secs(60));
    println!("Priority boost: standard (deadline: 60s)");
    println!("  - At 75% of deadline: boost by 1");
    println!("  - At 50% of deadline: boost by 2");
    println!("  - At 25% of deadline: boost by 3");
    println!("  - At 10% of deadline: boost by 4");

    let config = LaneConfig::new(1, 5).with_priority_boost(boost_config);

    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_lane("boosted", config, 5) // Start at priority 5
        .build()
        .await?;

    manager.start().await?;

    let cmd = Box::new(WorkCommand {
        id: "boost-1".to_string(),
    });
    let rx = manager.submit("boosted", cmd).await?;
    let _ = rx.await;

    println!("✓ Command executed with priority boosting enabled");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    println!();

    // Example 3: Queue Partitioning
    println!("=== Example 3: Queue Partitioning ===");

    // Auto-detect CPU cores
    let partition_config = PartitionConfig::auto();
    println!(
        "Partition config: {} partitions (auto-detected CPU cores)",
        partition_config.num_partitions
    );

    // Different partitioning strategies
    let round_robin = PartitionConfig::round_robin(4);
    println!("Round-robin: {} partitions", round_robin.num_partitions);

    let hash_based = PartitionConfig::hash(8);
    println!("Hash-based: {} partitions", hash_based.num_partitions);

    println!();

    // Example 4: Multi-core Parallelism
    println!("=== Example 4: Multi-core Parallelism ===");

    let distributed_queue = Arc::new(LocalDistributedQueue::auto());
    println!(
        "Local distributed queue: {} partitions",
        distributed_queue.num_partitions()
    );
    println!("Worker ID: {}", distributed_queue.worker_id());
    println!("Is coordinator: {}", distributed_queue.is_coordinator());

    // Demonstrate parallel processing
    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_lane("parallel", LaneConfig::new(1, 20), 0)
        .build()
        .await?;

    manager.start().await?;

    println!("\nSubmitting 50 commands for parallel processing...");
    let start = std::time::Instant::now();
    let mut receivers = Vec::new();

    for i in 0..50 {
        let cmd = Box::new(WorkCommand {
            id: format!("parallel-{}", i),
        });
        let rx = manager.submit("parallel", cmd).await?;
        receivers.push(rx);
    }

    // Wait for all to complete
    for rx in receivers {
        let _ = rx.await;
    }

    let elapsed = start.elapsed();
    println!("✓ Completed 50 commands in {:.2}s", elapsed.as_secs_f64());
    println!(
        "  Throughput: {:.1} commands/second",
        50.0 / elapsed.as_secs_f64()
    );

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    println!();
    println!("=== All Examples Complete ===");

    Ok(())
}
