//! Observability features example
//!
//! This example demonstrates:
//! - Metrics collection
//! - Latency histograms with percentiles
//! - Queue depth alerts
//! - Latency alerts

use a3s_lane::{
    AlertLevel, AlertManager, Command, EventEmitter, LaneConfig, QueueManagerBuilder,
    QueueMetrics, Result,
};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// A command with variable execution time
struct WorkCommand {
    id: String,
    duration_ms: u64,
}

#[async_trait]
impl Command for WorkCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        let start = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(self.duration_ms)).await;
        let elapsed = start.elapsed();

        Ok(serde_json::json!({
            "id": self.id,
            "duration_ms": elapsed.as_millis()
        }))
    }

    fn command_type(&self) -> &str {
        "work"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    println!("=== A3S Lane: Observability Features Example ===\n");

    // Create metrics collector
    let metrics = QueueMetrics::local();

    // Create alert manager with thresholds
    let alerts = Arc::new(AlertManager::with_queue_depth_alerts(
        10,  // Warning at 10 pending
        20,  // Critical at 20 pending
    ));

    // Add latency alerts
    alerts
        .set_latency_config(a3s_lane::LatencyAlertConfig::new(100.0, 500.0))
        .await;

    // Add alert callback
    let alert_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let alert_count_clone = Arc::clone(&alert_count);
    alerts
        .add_callback(move |alert| {
            alert_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let level_str = match alert.level {
                AlertLevel::Info => "INFO",
                AlertLevel::Warning => "WARN",
                AlertLevel::Critical => "CRIT",
            };
            println!(
                "  🚨 [{}] {} - {}",
                level_str, alert.lane_id, alert.message
            );
        })
        .await;

    let emitter = EventEmitter::new(100);

    // Build manager with metrics and alerts
    let manager = QueueManagerBuilder::new(emitter)
        .with_metrics(metrics.clone())
        .with_alerts(Arc::clone(&alerts))
        .with_lane("api", LaneConfig::new(1, 5), 0)
        .build()
        .await?;

    manager.start().await?;
    println!("✓ Queue manager started with metrics and alerts\n");

    // Example 1: Collect metrics
    println!("=== Example 1: Metrics Collection ===");

    // Submit commands with varying latencies
    let latencies = [50, 100, 150, 200, 250, 300, 400, 600];
    let mut receivers = Vec::new();

    for (i, latency) in latencies.iter().enumerate() {
        let cmd = Box::new(WorkCommand {
            id: format!("work-{}", i),
            duration_ms: *latency,
        });

        let start = std::time::Instant::now();
        let rx = manager.submit("api", cmd).await?;
        receivers.push((i, start, rx));

        // Record submission
        metrics.record_submit("api").await;
    }

    println!("Submitted {} commands", latencies.len());

    // Wait for results and record metrics
    for (i, start, rx) in receivers {
        match rx.await {
            Ok(Ok(_result)) => {
                let latency_ms = start.elapsed().as_millis() as f64;
                metrics.record_complete("api", latency_ms).await;

                // Check latency alerts
                alerts.check_latency("api", latency_ms).await;
            }
            Ok(Err(e)) => {
                metrics.record_failure("api").await;
                println!("✗ Command {} failed: {}", i, e);
            }
            Err(_) => {
                println!("✗ Command {} channel closed", i);
            }
        }
    }

    println!();

    // Display metrics
    println!("=== Metrics Summary ===");
    let snapshot = metrics.snapshot().await;

    if let Some(submitted) = snapshot.counters.get("lane.commands.submitted") {
        println!("Commands submitted: {}", submitted);
    }

    if let Some(completed) = snapshot.counters.get("lane.commands.completed") {
        println!("Commands completed: {}", completed);
    }

    if let Some(stats) = snapshot.histograms.get("lane.command.latency_ms") {
        println!("\nLatency Statistics:");
        println!("  Count: {}", stats.count);
        println!("  Min: {:.2}ms", stats.min);
        println!("  Max: {:.2}ms", stats.max);
        println!("  Mean: {:.2}ms", stats.mean);
        println!("  p50: {:.2}ms", stats.percentiles.p50);
        println!("  p90: {:.2}ms", stats.percentiles.p90);
        println!("  p95: {:.2}ms", stats.percentiles.p95);
        println!("  p99: {:.2}ms", stats.percentiles.p99);
    }

    println!();

    // Example 2: Queue depth alerts
    println!("=== Example 2: Queue Depth Alerts ===");

    // Simulate queue depth changes
    println!("Simulating queue depth changes...");
    alerts.check_queue_depth("api", 5).await; // No alert
    alerts.check_queue_depth("api", 12).await; // Warning
    alerts.check_queue_depth("api", 25).await; // Critical
    alerts.check_queue_depth("api", 8).await; // Back to normal

    println!();

    // Display alert statistics
    let alert_total = alert_count.load(std::sync::atomic::Ordering::SeqCst);
    println!("Total alerts triggered: {}", alert_total);

    println!();

    // Example 3: Per-lane metrics
    println!("=== Example 3: Per-Lane Metrics ===");

    if let Some(submitted) = snapshot
        .counters
        .get("lane.commands.submitted.api")
    {
        println!("API lane - Commands submitted: {}", submitted);
    }

    if let Some(stats) = snapshot
        .histograms
        .get("lane.command.latency_ms.api")
    {
        println!("API lane - Mean latency: {:.2}ms", stats.mean);
    }

    println!();

    // Graceful shutdown
    println!("=== Shutting Down ===");
    manager.shutdown().await;
    manager.drain(Duration::from_secs(5)).await?;
    println!("✓ Shutdown complete");

    Ok(())
}
