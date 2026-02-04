//! Basic usage example
//!
//! This example demonstrates the basic usage of a3s-lane:
//! - Creating a queue manager with default lanes
//! - Submitting commands
//! - Receiving results

use a3s_lane::{Command, EventEmitter, QueueManagerBuilder, Result};
use async_trait::async_trait;

/// A simple command that processes a string
struct GreetCommand {
    name: String,
}

#[async_trait]
impl Command for GreetCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        // Simulate some work
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        Ok(serde_json::json!({
            "greeting": format!("Hello, {}!", self.name),
            "timestamp": chrono::Utc::now().to_rfc3339()
        }))
    }

    fn command_type(&self) -> &str {
        "greet"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    println!("=== A3S Lane: Basic Usage Example ===\n");

    // Create event emitter
    let emitter = EventEmitter::new(100);

    // Build queue manager with default lanes
    let manager = QueueManagerBuilder::new(emitter)
        .with_default_lanes()
        .build()
        .await?;

    println!("✓ Queue manager created with 6 default lanes");

    // Start the scheduler
    manager.start().await?;
    println!("✓ Scheduler started\n");

    // Submit commands to different lanes
    println!("Submitting commands...");

    let names = vec!["Alice", "Bob", "Charlie"];
    let mut receivers = Vec::new();

    for name in names {
        let cmd = Box::new(GreetCommand {
            name: name.to_string(),
        });

        // Submit to query lane (priority 2)
        let rx = manager.submit("query", cmd).await?;
        receivers.push((name, rx));

        println!("  → Submitted command for {}", name);
    }

    println!("\nWaiting for results...\n");

    // Collect results
    for (name, rx) in receivers {
        match rx.await {
            Ok(Ok(result)) => {
                println!("✓ Result for {}: {}", name, result);
            }
            Ok(Err(e)) => {
                eprintln!("✗ Error for {}: {}", name, e);
            }
            Err(e) => {
                eprintln!("✗ Channel error for {}: {}", name, e);
            }
        }
    }

    // Get statistics
    println!("\n=== Queue Statistics ===");
    let stats = manager.stats().await?;
    println!("Total pending: {}", stats.total_pending);
    println!("Total active: {}", stats.total_active);
    println!("Lanes: {}", stats.lanes.len());

    // Graceful shutdown
    println!("\n=== Shutting Down ===");
    manager.shutdown().await;
    manager
        .drain(tokio::time::Duration::from_secs(5))
        .await?;
    println!("✓ Shutdown complete");

    Ok(())
}
