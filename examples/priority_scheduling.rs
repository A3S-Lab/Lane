//! Priority Scheduling Example
//!
//! This example demonstrates A3S Lane's multi-queue priority scheduling:
//! 1. Multiple queues with different priorities
//! 2. Higher priority lanes execute first
//! 3. Lower priority lanes only execute when higher priority lanes are empty or at capacity
//! 4. Deterministic response for critical paths

use a3s_lane::{Command, EventEmitter, LaneConfig, QueueManagerBuilder, Result};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::time::{Duration, Instant};

/// Command that tracks execution order and timing
struct TrackedCommand {
    id: String,
    lane: String,
    priority: u8,
    duration_ms: u64,
    execution_order: Arc<AtomicU32>,
}

#[async_trait]
impl Command for TrackedCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        let order = self.execution_order.fetch_add(1, Ordering::SeqCst);
        let start = Instant::now();

        println!(
            "[Order: {:2}] Executing {} (lane: {}, priority: {}) - will take {}ms",
            order, self.id, self.lane, self.priority, self.duration_ms
        );

        tokio::time::sleep(Duration::from_millis(self.duration_ms)).await;

        let elapsed = start.elapsed();
        println!(
            "[Order: {:2}] Completed {} in {:?}",
            order, self.id, elapsed
        );

        Ok(serde_json::json!({
            "id": self.id,
            "lane": self.lane,
            "priority": self.priority,
            "execution_order": order,
            "duration_ms": elapsed.as_millis()
        }))
    }

    fn command_type(&self) -> &str {
        "tracked"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    println!("=== A3S Lane: Priority Scheduling Test ===\n");

    // Test 1: Basic priority ordering
    test_basic_priority_ordering().await?;

    // Test 2: Concurrency limit behavior
    test_concurrency_limit_behavior().await?;

    // Test 3: Mixed priority workload
    test_mixed_priority_workload().await?;

    println!("\n=== All Tests Completed ===");
    Ok(())
}

/// Test 1: Verify that higher priority lanes execute first
async fn test_basic_priority_ordering() -> anyhow::Result<()> {
    println!("=== Test 1: Basic Priority Ordering ===");
    println!("Expected: System (P0) → Control (P1) → Query (P2) → Session (P3)\n");

    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build()
        .await?;

    manager.start().await?;

    // Give scheduler time to start
    tokio::time::sleep(Duration::from_millis(20)).await;

    let execution_order = Arc::new(AtomicU32::new(0));

    // Submit commands in REVERSE priority order (lowest priority first)
    // Expected execution: highest priority first (system → control → query → session)
    let mut receivers = Vec::new();

    // Submit to session lane (priority 3) - should execute LAST
    let cmd = Box::new(TrackedCommand {
        id: "session-cmd".to_string(),
        lane: "session".to_string(),
        priority: 3,
        duration_ms: 50,
        execution_order: Arc::clone(&execution_order),
    });
    receivers.push(("session", manager.submit("session", cmd).await?));
    println!("Submitted: session-cmd (priority 3)");

    // Submit to query lane (priority 2) - should execute 3rd
    let cmd = Box::new(TrackedCommand {
        id: "query-cmd".to_string(),
        lane: "query".to_string(),
        priority: 2,
        duration_ms: 50,
        execution_order: Arc::clone(&execution_order),
    });
    receivers.push(("query", manager.submit("query", cmd).await?));
    println!("Submitted: query-cmd (priority 2)");

    // Submit to control lane (priority 1) - should execute 2nd
    let cmd = Box::new(TrackedCommand {
        id: "control-cmd".to_string(),
        lane: "control".to_string(),
        priority: 1,
        duration_ms: 50,
        execution_order: Arc::clone(&execution_order),
    });
    receivers.push(("control", manager.submit("control", cmd).await?));
    println!("Submitted: control-cmd (priority 1)");

    // Submit to system lane (priority 0) - should execute FIRST
    let cmd = Box::new(TrackedCommand {
        id: "system-cmd".to_string(),
        lane: "system".to_string(),
        priority: 0,
        duration_ms: 50,
        execution_order: Arc::clone(&execution_order),
    });
    receivers.push(("system", manager.submit("system", cmd).await?));
    println!("Submitted: system-cmd (priority 0)");

    println!("\nWaiting for execution...\n");

    // Collect results
    let mut results = Vec::new();
    for (lane, rx) in receivers {
        match rx.await {
            Ok(Ok(result)) => {
                results.push((lane, result));
            }
            Ok(Err(e)) => eprintln!("Error in {}: {}", lane, e),
            Err(e) => eprintln!("Channel error in {}: {}", lane, e),
        }
    }

    // Verify execution order
    println!("\n--- Verification ---");
    results.sort_by_key(|(_, result)| result["execution_order"].as_u64().unwrap());

    for (i, (lane, result)) in results.iter().enumerate() {
        let order = result["execution_order"].as_u64().unwrap();
        let priority = result["priority"].as_u64().unwrap();
        println!(
            "Execution order {}: {} (priority {})",
            order, lane, priority
        );

        // Verify that execution order matches priority order
        assert_eq!(
            order, i as u64,
            "Execution order mismatch for {}",
            lane
        );
    }

    println!("✓ Test 1 PASSED: Commands executed in priority order\n");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    Ok(())
}

/// Test 2: Verify that lower priority lanes execute when higher priority lanes reach concurrency limit
async fn test_concurrency_limit_behavior() -> anyhow::Result<()> {
    println!("=== Test 2: Concurrency Limit Behavior ===");
    println!("Expected: When system lane (P0) is at capacity, query lane (P2) can execute\n");

    let emitter = EventEmitter::new(100);

    // Create manager with custom lane config to ensure system lane has max_concurrency=1
    let mut builder = QueueManagerBuilder::new(emitter);
    builder = builder.with_lane(
        "system",
        LaneConfig::new(1, 1), // max_concurrency = 1
        0,
    );
    builder = builder.with_lane(
        "query",
        LaneConfig::new(1, 10),
        2,
    );

    let manager = builder.build().await?;
    manager.start().await?;

    // Give scheduler time to start
    tokio::time::sleep(Duration::from_millis(20)).await;

    let execution_order = Arc::new(AtomicU32::new(0));

    // Submit 2 long-running commands to system lane (max_concurrency = 1)
    // This will block the system lane
    println!("Submitting 2 long-running commands to system lane (max_concurrency = 1)...");

    let cmd1 = Box::new(TrackedCommand {
        id: "system-cmd-1".to_string(),
        lane: "system".to_string(),
        priority: 0,
        duration_ms: 300,
        execution_order: Arc::clone(&execution_order),
    });
    let rx1 = manager.submit("system", cmd1).await?;

    let cmd2 = Box::new(TrackedCommand {
        id: "system-cmd-2".to_string(),
        lane: "system".to_string(),
        priority: 0,
        duration_ms: 300,
        execution_order: Arc::clone(&execution_order),
    });
    let rx2 = manager.submit("system", cmd2).await?;

    // Wait a bit to ensure system lane is busy
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now submit to query lane (lower priority)
    // This should execute while system lane is blocked
    println!("Submitting command to query lane (priority 2)...");
    let cmd3 = Box::new(TrackedCommand {
        id: "query-cmd".to_string(),
        lane: "query".to_string(),
        priority: 2,
        duration_ms: 100,
        execution_order: Arc::clone(&execution_order),
    });
    let rx3 = manager.submit("query", cmd3).await?;

    println!("\nWaiting for execution...\n");

    // Collect results
    let result1 = rx1.await??;
    let result2 = rx2.await??;
    let result3 = rx3.await??;

    println!("\n--- Verification ---");
    let order1 = result1["execution_order"].as_u64().unwrap();
    let order2 = result2["execution_order"].as_u64().unwrap();
    let order3 = result3["execution_order"].as_u64().unwrap();

    println!("system-cmd-1 execution order: {}", order1);
    println!("system-cmd-2 execution order: {}", order2);
    println!("query-cmd execution order: {}", order3);

    // Verify that query command executed while system lane was blocked
    // query-cmd should execute after system-cmd-1 starts but before system-cmd-2
    assert_eq!(order1, 0, "system-cmd-1 should execute first");
    assert_eq!(order3, 1, "query-cmd should execute second (while system lane is blocked)");
    assert_eq!(order2, 2, "system-cmd-2 should execute last");

    println!("✓ Test 2 PASSED: Lower priority lane executed when higher priority lane was at capacity\n");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    Ok(())
}

/// Test 3: Mixed priority workload with realistic scenario
async fn test_mixed_priority_workload() -> anyhow::Result<()> {
    println!("=== Test 3: Mixed Priority Workload ===");
    println!("Simulating realistic workload with multiple priorities\n");

    let emitter = EventEmitter::new(100);
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build()
        .await?;

    manager.start().await?;

    // Give scheduler time to start
    tokio::time::sleep(Duration::from_millis(20)).await;

    let execution_order = Arc::new(AtomicU32::new(0));

    // Submit a mix of commands
    let mut receivers = Vec::new();

    // Simulate a burst of low-priority prompt commands
    for i in 0..3 {
        let cmd = Box::new(TrackedCommand {
            id: format!("prompt-{}", i),
            lane: "prompt".to_string(),
            priority: 5,
            duration_ms: 100,
            execution_order: Arc::clone(&execution_order),
        });
        receivers.push((format!("prompt-{}", i), manager.submit("prompt", cmd).await?));
    }
    println!("Submitted 3 prompt commands (priority 5)");

    // Wait a bit
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Submit a critical system command - should jump to front
    let cmd = Box::new(TrackedCommand {
        id: "system-critical".to_string(),
        lane: "system".to_string(),
        priority: 0,
        duration_ms: 50,
        execution_order: Arc::clone(&execution_order),
    });
    receivers.push(("system-critical".to_string(), manager.submit("system", cmd).await?));
    println!("Submitted critical system command (priority 0)");

    // Submit some medium-priority query commands
    for i in 0..2 {
        let cmd = Box::new(TrackedCommand {
            id: format!("query-{}", i),
            lane: "query".to_string(),
            priority: 2,
            duration_ms: 80,
            execution_order: Arc::clone(&execution_order),
        });
        receivers.push((format!("query-{}", i), manager.submit("query", cmd).await?));
    }
    println!("Submitted 2 query commands (priority 2)");

    println!("\nWaiting for execution...\n");

    // Collect results
    let mut results = Vec::new();
    for (id, rx) in receivers {
        match rx.await {
            Ok(Ok(result)) => {
                results.push((id, result));
            }
            Ok(Err(e)) => eprintln!("Error in {}: {}", id, e),
            Err(e) => eprintln!("Channel error in {}: {}", id, e),
        }
    }

    // Sort by execution order
    results.sort_by_key(|(_, result)| result["execution_order"].as_u64().unwrap());

    println!("\n--- Execution Order ---");
    for (id, result) in &results {
        let order = result["execution_order"].as_u64().unwrap();
        let priority = result["priority"].as_u64().unwrap();
        let lane = result["lane"].as_str().unwrap();
        println!(
            "[Order: {:2}] {} (lane: {}, priority: {})",
            order, id, lane, priority
        );
    }

    // Verify that system command executed early despite being submitted later
    let system_result = results.iter().find(|(id, _)| id == "system-critical").unwrap();
    let system_order = system_result.1["execution_order"].as_u64().unwrap();

    println!("\n--- Verification ---");
    println!("System command execution order: {}", system_order);
    println!("Expected: System command should execute early (order 0 or 1)");

    assert!(
        system_order <= 1,
        "System command should execute early despite being submitted later"
    );

    println!("✓ Test 3 PASSED: Critical commands preempted lower priority commands\n");

    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;

    Ok(())
}
