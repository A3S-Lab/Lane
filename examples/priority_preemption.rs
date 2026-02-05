//! Priority Preemption Example
//!
//! This example demonstrates the core scheduling rule:
//! "只有当前最高优先级队列为空或达到并发上限时，才会下探到下一级"
//! (Only explores the next level when the current highest priority queue is empty or reaches concurrency limit)
//!
//! Test scenarios:
//! 1. High priority queue empty → Low priority executes
//! 2. High priority queue at capacity → Low priority executes
//! 3. High priority queue has pending + capacity → Low priority waits

use a3s_lane::{Command, EventEmitter, LaneConfig, QueueManagerBuilder, Result};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::time::{Duration, Instant};

/// Simple command with timing
struct TimedCommand {
    id: String,
    duration_ms: u64,
    counter: Arc<AtomicU32>,
}

#[async_trait]
impl Command for TimedCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        let order = self.counter.fetch_add(1, Ordering::SeqCst);
        let start = Instant::now();

        println!(
            "[{:?}] [Order: {}] START: {} (duration: {}ms)",
            start.elapsed(),
            order,
            self.id,
            self.duration_ms
        );

        tokio::time::sleep(Duration::from_millis(self.duration_ms)).await;

        println!(
            "[{:?}] [Order: {}] END: {}",
            start.elapsed(),
            order,
            self.id
        );

        Ok(serde_json::json!({
            "id": self.id,
            "order": order,
            "duration_ms": self.duration_ms
        }))
    }

    fn command_type(&self) -> &str {
        "timed"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    println!("=== A3S Lane: Priority Preemption Test ===\n");

    // Scenario 1: High priority queue empty
    scenario_1_high_priority_empty().await?;

    // Scenario 2: High priority queue at capacity
    scenario_2_high_priority_at_capacity().await?;

    // Scenario 3: High priority queue has pending + capacity
    scenario_3_high_priority_has_pending_and_capacity().await?;

    println!("\n=== All Scenarios Completed ===");
    Ok(())
}

/// Scenario 1: When high priority queue is EMPTY, low priority executes
async fn scenario_1_high_priority_empty() -> anyhow::Result<()> {
    println!("=== Scenario 1: High Priority Queue Empty ===");
    println!("Setup: No commands in high priority queue");
    println!("Expected: Low priority commands execute immediately\n");

    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build()
        .await?;

    manager.start().await?;

    let counter = Arc::new(AtomicU32::new(0));

    // Submit only to low priority queue (prompt, priority 5)
    println!("Submitting 2 commands to prompt lane (priority 5)...");

    let cmd1 = Box::new(TimedCommand {
        id: "prompt-1".to_string(),
        duration_ms: 100,
        counter: Arc::clone(&counter),
    });
    let rx1 = manager.submit("prompt", cmd1).await?;

    let cmd2 = Box::new(TimedCommand {
        id: "prompt-2".to_string(),
        duration_ms: 100,
        counter: Arc::clone(&counter),
    });
    let rx2 = manager.submit("prompt", cmd2).await?;

    println!("\nWaiting for execution...\n");

    let result1 = rx1.await??;
    let result2 = rx2.await??;

    println!("\n--- Verification ---");
    println!("Result 1: {}", result1);
    println!("Result 2: {}", result2);
    println!("✓ Scenario 1 PASSED: Low priority commands executed when high priority queue was empty\n");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    Ok(())
}

/// Scenario 2: When high priority queue is AT CAPACITY, low priority executes
async fn scenario_2_high_priority_at_capacity() -> anyhow::Result<()> {
    println!("=== Scenario 2: High Priority Queue At Capacity ===");
    println!("Setup: High priority queue (system) at max_concurrency (1)");
    println!("Expected: Low priority commands execute while high priority is blocked\n");

    let emitter = EventEmitter::new(100);

    // Create manager with custom lane config
    let mut builder = QueueManagerBuilder::new(emitter);

    // System lane with max_concurrency = 1
    builder = builder.with_lane(
        "system",
        LaneConfig::new(1, 1), // min=1, max=1
        0,                     // priority 0 (highest)
    );

    // Prompt lane with max_concurrency = 4
    builder = builder.with_lane(
        "prompt",
        LaneConfig::new(1, 4),
        5, // priority 5 (lowest)
    );

    let manager = builder.build().await?;
    manager.start().await?;

    let counter = Arc::new(AtomicU32::new(0));

    // Submit 2 long-running commands to system lane (max_concurrency = 1)
    println!("Submitting 2 commands to system lane (max_concurrency = 1)...");

    let cmd1 = Box::new(TimedCommand {
        id: "system-1".to_string(),
        duration_ms: 300,
        counter: Arc::clone(&counter),
    });
    let rx1 = manager.submit("system", cmd1).await?;

    let cmd2 = Box::new(TimedCommand {
        id: "system-2".to_string(),
        duration_ms: 300,
        counter: Arc::clone(&counter),
    });
    let rx2 = manager.submit("system", cmd2).await?;

    // Wait for system lane to be busy
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Submit to low priority queue
    println!("Submitting command to prompt lane (priority 5)...");

    let cmd3 = Box::new(TimedCommand {
        id: "prompt-1".to_string(),
        duration_ms: 100,
        counter: Arc::clone(&counter),
    });
    let rx3 = manager.submit("prompt", cmd3).await?;

    println!("\nWaiting for execution...\n");

    let result1 = rx1.await??;
    let result2 = rx2.await??;
    let result3 = rx3.await??;

    println!("\n--- Verification ---");
    let order1 = result1["order"].as_u64().unwrap();
    let order2 = result2["order"].as_u64().unwrap();
    let order3 = result3["order"].as_u64().unwrap();

    println!("system-1 order: {}", order1);
    println!("prompt-1 order: {}", order3);
    println!("system-2 order: {}", order2);

    // Verify execution order
    assert_eq!(order1, 0, "system-1 should execute first");
    assert_eq!(
        order3, 1,
        "prompt-1 should execute second (while system is at capacity)"
    );
    assert_eq!(order2, 2, "system-2 should execute last");

    println!("✓ Scenario 2 PASSED: Low priority executed when high priority was at capacity\n");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    Ok(())
}

/// Scenario 3: When high priority queue HAS PENDING + CAPACITY, low priority WAITS
async fn scenario_3_high_priority_has_pending_and_capacity() -> anyhow::Result<()> {
    println!("=== Scenario 3: High Priority Has Pending + Capacity ===");
    println!("Setup: High priority queue has pending commands AND available capacity");
    println!("Expected: High priority commands execute first, low priority waits\n");

    let emitter = EventEmitter::new(100);

    let mut builder = QueueManagerBuilder::new(emitter);

    // System lane with max_concurrency = 4 (plenty of capacity)
    builder = builder.with_lane(
        "system",
        LaneConfig::new(1, 4),
        0, // priority 0 (highest)
    );

    // Prompt lane with max_concurrency = 4
    builder = builder.with_lane(
        "prompt",
        LaneConfig::new(1, 4),
        5, // priority 5 (lowest)
    );

    let manager = builder.build().await?;

    let counter = Arc::new(AtomicU32::new(0));

    // Submit all commands BEFORE starting scheduler
    // This ensures they're all queued before any execution begins
    println!("Submitting command to prompt lane (priority 5)...");

    let cmd_low = Box::new(TimedCommand {
        id: "prompt-1".to_string(),
        duration_ms: 100,
        counter: Arc::clone(&counter),
    });
    let rx_low = manager.submit("prompt", cmd_low).await?;

    // Immediately submit to high priority
    println!("Submitting 2 commands to system lane (priority 0)...");

    let cmd_high1 = Box::new(TimedCommand {
        id: "system-1".to_string(),
        duration_ms: 100,
        counter: Arc::clone(&counter),
    });
    let rx_high1 = manager.submit("system", cmd_high1).await?;

    let cmd_high2 = Box::new(TimedCommand {
        id: "system-2".to_string(),
        duration_ms: 100,
        counter: Arc::clone(&counter),
    });
    let rx_high2 = manager.submit("system", cmd_high2).await?;

    // NOW start the scheduler - all commands are already queued
    println!("Starting scheduler with all commands queued...");
    manager.start().await?;

    println!("\nWaiting for execution...\n");

    let result_low = rx_low.await??;
    let result_high1 = rx_high1.await??;
    let result_high2 = rx_high2.await??;

    println!("\n--- Verification ---");
    let order_low = result_low["order"].as_u64().unwrap();
    let order_high1 = result_high1["order"].as_u64().unwrap();
    let order_high2 = result_high2["order"].as_u64().unwrap();

    println!("prompt-1 order: {}", order_low);
    println!("system-1 order: {}", order_high1);
    println!("system-2 order: {}", order_high2);

    // Verify that high priority commands executed before low priority
    // Even though low priority was submitted first
    assert!(
        order_high1 < order_low,
        "system-1 should execute before prompt-1"
    );
    assert!(
        order_high2 < order_low,
        "system-2 should execute before prompt-1"
    );

    println!("✓ Scenario 3 PASSED: High priority commands executed first despite being submitted later\n");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    Ok(())
}
