//! Performance benchmarks for a3s-lane
//!
//! Run with: cargo bench

use a3s_lane::{Command, EventEmitter, LaneConfig, QueueManagerBuilder, Result};
use async_trait::async_trait;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::time::Duration;
use tokio::runtime::Runtime;

/// A minimal command for benchmarking
struct BenchCommand {
    id: usize,
}

#[async_trait]
impl Command for BenchCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        // Minimal work to measure queue overhead
        Ok(serde_json::json!({"id": self.id}))
    }

    fn command_type(&self) -> &str {
        "bench"
    }
}

/// A command with simulated work
struct WorkCommand {
    id: usize,
    work_us: u64,
}

#[async_trait]
impl Command for WorkCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        tokio::time::sleep(Duration::from_micros(self.work_us)).await;
        Ok(serde_json::json!({"id": self.id}))
    }

    fn command_type(&self) -> &str {
        "work"
    }
}

fn bench_submit_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("submit_throughput");

    for size in [10, 100, 1000].iter() {
        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let emitter = EventEmitter::new(1000);
                let manager = QueueManagerBuilder::new(emitter)
                    .with_lane("bench", LaneConfig::new(1, 10), 0)
                    .build()
                    .await
                    .unwrap();

                manager.start().await.unwrap();

                let mut receivers = Vec::new();
                for i in 0..size {
                    let cmd = Box::new(BenchCommand { id: i });
                    let rx = manager.submit("bench", cmd).await.unwrap();
                    receivers.push(rx);
                }

                for rx in receivers {
                    let _ = rx.await;
                }

                manager.shutdown().await;
                manager.drain(Duration::from_secs(5)).await.unwrap();
            });
        });
    }

    group.finish();
}

fn bench_concurrent_execution(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("concurrent_execution");

    for concurrency in [1, 5, 10, 20].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(concurrency),
            concurrency,
            |b, &concurrency| {
                b.to_async(&rt).iter(|| async move {
                    let emitter = EventEmitter::new(1000);
                    let manager = QueueManagerBuilder::new(emitter)
                        .with_lane("bench", LaneConfig::new(1, concurrency), 0)
                        .build()
                        .await
                        .unwrap();

                    manager.start().await.unwrap();

                    let mut receivers = Vec::new();
                    for i in 0..100 {
                        let cmd = Box::new(WorkCommand {
                            id: i,
                            work_us: 100, // 100 microseconds of work
                        });
                        let rx = manager.submit("bench", cmd).await.unwrap();
                        receivers.push(rx);
                    }

                    for rx in receivers {
                        let _ = rx.await;
                    }

                    manager.shutdown().await;
                    manager.drain(Duration::from_secs(5)).await.unwrap();
                });
            },
        );
    }

    group.finish();
}

fn bench_priority_scheduling(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("priority_scheduling", |b| {
        b.to_async(&rt).iter(|| async {
            let emitter = EventEmitter::new(1000);
            let manager = QueueManagerBuilder::new(emitter)
                .with_lane("high", LaneConfig::new(1, 5), 0) // High priority
                .with_lane("medium", LaneConfig::new(1, 5), 5) // Medium priority
                .with_lane("low", LaneConfig::new(1, 5), 10) // Low priority
                .build()
                .await
                .unwrap();

            manager.start().await.unwrap();

            let mut receivers = Vec::new();

            // Submit to all lanes
            for i in 0..30 {
                let lane = match i % 3 {
                    0 => "high",
                    1 => "medium",
                    _ => "low",
                };
                let cmd = Box::new(BenchCommand { id: i });
                let rx = manager.submit(lane, cmd).await.unwrap();
                receivers.push(rx);
            }

            for rx in receivers {
                let _ = rx.await;
            }

            manager.shutdown().await;
            manager.drain(Duration::from_secs(5)).await.unwrap();
        });
    });
}

fn bench_metrics_overhead(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("metrics_overhead");

    // Without metrics
    group.bench_function("without_metrics", |b| {
        b.to_async(&rt).iter(|| async {
            let emitter = EventEmitter::new(1000);
            let manager = QueueManagerBuilder::new(emitter)
                .with_lane("bench", LaneConfig::new(1, 10), 0)
                .build()
                .await
                .unwrap();

            manager.start().await.unwrap();

            let mut receivers = Vec::new();
            for i in 0..100 {
                let cmd = Box::new(BenchCommand { id: i });
                let rx = manager.submit("bench", cmd).await.unwrap();
                receivers.push(rx);
            }

            for rx in receivers {
                let _ = rx.await;
            }

            manager.shutdown().await;
            manager.drain(Duration::from_secs(5)).await.unwrap();
        });
    });

    // With metrics
    group.bench_function("with_metrics", |b| {
        b.to_async(&rt).iter(|| async {
            let emitter = EventEmitter::new(1000);
            let metrics = a3s_lane::QueueMetrics::local();

            let manager = QueueManagerBuilder::new(emitter)
                .with_metrics(metrics.clone())
                .with_lane("bench", LaneConfig::new(1, 10), 0)
                .build()
                .await
                .unwrap();

            manager.start().await.unwrap();

            let mut receivers = Vec::new();
            for i in 0..100 {
                let cmd = Box::new(BenchCommand { id: i });
                let rx = manager.submit("bench", cmd).await.unwrap();
                receivers.push(rx);
            }

            for rx in receivers {
                let _ = rx.await;
            }

            manager.shutdown().await;
            manager.drain(Duration::from_secs(5)).await.unwrap();
        });
    });

    group.finish();
}

fn bench_rate_limiting(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("rate_limiting", |b| {
        b.to_async(&rt).iter(|| async {
            let emitter = EventEmitter::new(1000);
            let config =
                LaneConfig::new(1, 10).with_rate_limit(a3s_lane::RateLimitConfig::per_second(50));

            let manager = QueueManagerBuilder::new(emitter)
                .with_lane("bench", config, 0)
                .build()
                .await
                .unwrap();

            manager.start().await.unwrap();

            let mut receivers = Vec::new();
            for i in 0..100 {
                let cmd = Box::new(BenchCommand { id: i });
                let rx = manager.submit("bench", cmd).await.unwrap();
                receivers.push(rx);
            }

            for rx in receivers {
                let _ = rx.await;
            }

            manager.shutdown().await;
            manager.drain(Duration::from_secs(5)).await.unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_submit_throughput,
    bench_concurrent_execution,
    bench_priority_scheduling,
    bench_metrics_overhead,
    bench_rate_limiting
);
criterion_main!(benches);
