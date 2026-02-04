//! Reliability features example
//!
//! This example demonstrates:
//! - Command timeout
//! - Retry policies with exponential backoff
//! - Dead letter queue
//! - Graceful shutdown

use a3s_lane::{Command, EventEmitter, LaneConfig, QueueManagerBuilder, Result, RetryPolicy};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A command that fails a few times before succeeding
struct RetryableCommand {
    id: String,
    attempts: Arc<AtomicU32>,
    max_failures: u32,
}

#[async_trait]
impl Command for RetryableCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);

        println!(
            "  [{}] Attempt {} (will fail {} times)",
            self.id, attempt, self.max_failures
        );

        if attempt < self.max_failures {
            // Simulate failure
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(a3s_lane::LaneError::Other(format!(
                "Temporary failure (attempt {})",
                attempt
            )));
        }

        // Success after retries
        tokio::time::sleep(Duration::from_millis(100)).await;
        Ok(serde_json::json!({
            "id": self.id,
            "attempts": attempt + 1,
            "status": "success"
        }))
    }

    fn command_type(&self) -> &str {
        "retryable"
    }
}

/// A command that times out
struct SlowCommand {
    id: String,
    delay_ms: u64,
}

#[async_trait]
impl Command for SlowCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        println!("  [{}] Starting (will take {}ms)", self.id, self.delay_ms);
        tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        Ok(serde_json::json!({
            "id": self.id,
            "status": "completed"
        }))
    }

    fn command_type(&self) -> &str {
        "slow"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    println!("=== A3S Lane: Reliability Features Example ===\n");

    let emitter = EventEmitter::new(100);

    // Configure lane with timeout and retry policy
    // exponential(3) uses default: 100ms initial delay, 30s max delay, 2x multiplier
    let retry_policy = RetryPolicy::exponential(3);

    let config = LaneConfig::new(1, 5)
        .with_timeout(Duration::from_millis(500))
        .with_retry_policy(retry_policy);

    // Build manager with DLQ
    let manager = QueueManagerBuilder::new(emitter)
        .with_lane("api", config, 0)
        .with_dlq(100)
        .build()
        .await?;

    manager.start().await?;
    println!("✓ Queue manager started with timeout and retry policy\n");

    // Example 1: Retry with exponential backoff
    println!("=== Example 1: Retry with Exponential Backoff ===");
    let attempts = Arc::new(AtomicU32::new(0));
    let cmd = Box::new(RetryableCommand {
        id: "retry-1".to_string(),
        attempts: Arc::clone(&attempts),
        max_failures: 2, // Will succeed on 3rd attempt
    });

    let rx = manager.submit("api", cmd).await?;
    match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(Ok(result))) => {
            println!("✓ Command succeeded after retries: {}", result);
        }
        Ok(Ok(Err(e))) => {
            println!("✗ Command failed: {}", e);
        }
        Ok(Err(_)) => {
            println!("✗ Channel closed");
        }
        Err(_) => {
            println!("✗ Timeout waiting for result");
        }
    }

    println!();

    // Example 2: Command timeout
    println!("=== Example 2: Command Timeout ===");
    let cmd = Box::new(SlowCommand {
        id: "slow-1".to_string(),
        delay_ms: 1000, // Will timeout (limit is 500ms)
    });

    let rx = manager.submit("api", cmd).await?;
    match tokio::time::timeout(Duration::from_secs(2), rx).await {
        Ok(Ok(Ok(result))) => {
            println!("✓ Command completed: {}", result);
        }
        Ok(Ok(Err(e))) => {
            println!("✓ Command timed out as expected: {}", e);
        }
        Ok(Err(_)) => {
            println!("✗ Channel closed");
        }
        Err(_) => {
            println!("✗ Timeout waiting for result");
        }
    }

    println!();

    // Example 3: Dead letter queue
    println!("=== Example 3: Dead Letter Queue ===");
    let attempts = Arc::new(AtomicU32::new(0));
    let cmd = Box::new(RetryableCommand {
        id: "dlq-1".to_string(),
        attempts: Arc::clone(&attempts),
        max_failures: 10, // Will exhaust retries and go to DLQ
    });

    let rx = manager.submit("api", cmd).await?;
    match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(Ok(result))) => {
            println!("✓ Command succeeded: {}", result);
        }
        Ok(Ok(Err(e))) => {
            println!("✓ Command failed and sent to DLQ: {}", e);
        }
        Ok(Err(_)) => {
            println!("✗ Channel closed");
        }
        Err(_) => {
            println!("✗ Timeout waiting for result");
        }
    }

    // Check DLQ
    tokio::time::sleep(Duration::from_millis(500)).await;
    let stats = manager.stats().await?;
    println!("  Dead letter count: {}", stats.dead_letter_count);

    if let Some(dlq) = manager.queue().dlq() {
        let letters = dlq.list().await;
        for letter in letters {
            println!(
                "  → DLQ entry: {} (type: {}, attempts: {})",
                letter.command_id, letter.command_type, letter.attempts
            );
        }
    }

    println!();

    // Example 4: Graceful shutdown
    println!("=== Example 4: Graceful Shutdown ===");
    println!("Initiating shutdown...");
    manager.shutdown().await;

    // Try to submit after shutdown (should fail)
    let cmd = Box::new(SlowCommand {
        id: "after-shutdown".to_string(),
        delay_ms: 100,
    });

    match manager.submit("api", cmd).await {
        Ok(_) => println!("✗ Unexpected: command accepted after shutdown"),
        Err(e) => println!("✓ Command rejected after shutdown: {}", e),
    }

    // Drain pending commands
    println!("Draining pending commands...");
    manager.drain(Duration::from_secs(5)).await?;
    println!("✓ Shutdown complete");

    Ok(())
}
