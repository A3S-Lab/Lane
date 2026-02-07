//! Metrics collection and reporting for queue observability.
//!
//! This module provides a pluggable metrics system with a local in-memory
//! implementation by default, but allows users to integrate external metrics
//! systems like Prometheus or OpenTelemetry.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A pluggable metrics backend trait.
///
/// Implement this trait to integrate with external metrics systems like
/// Prometheus, OpenTelemetry, or custom monitoring solutions.
#[async_trait]
pub trait MetricsBackend: Send + Sync {
    /// Increment a counter metric by the given value
    async fn increment_counter(&self, name: &str, value: u64);

    /// Set a gauge metric to the given value
    async fn set_gauge(&self, name: &str, value: f64);

    /// Record a histogram observation (typically for latency measurements in milliseconds)
    async fn record_histogram(&self, name: &str, value: f64);

    /// Get current counter value (for testing/debugging)
    async fn get_counter(&self, name: &str) -> Option<u64>;

    /// Get current gauge value (for testing/debugging)
    async fn get_gauge(&self, name: &str) -> Option<f64>;

    /// Get histogram statistics (for testing/debugging)
    async fn get_histogram_stats(&self, name: &str) -> Option<HistogramStats>;

    /// Reset all metrics (useful for testing)
    async fn reset(&self);

    /// Export all metrics as a snapshot
    async fn snapshot(&self) -> MetricsSnapshot;
}

/// Statistics for a histogram metric
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramStats {
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    /// Percentile values (p50, p90, p95, p99)
    pub percentiles: HistogramPercentiles,
}

/// Percentile values for histogram
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistogramPercentiles {
    pub p50: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
}

impl Default for HistogramStats {
    fn default() -> Self {
        Self::new()
    }
}

impl HistogramStats {
    pub fn new() -> Self {
        Self {
            count: 0,
            sum: 0.0,
            min: f64::MAX,
            max: f64::MIN,
            mean: 0.0,
            percentiles: HistogramPercentiles::default(),
        }
    }
}

/// Internal histogram data structure that tracks values for percentile calculation
#[derive(Debug, Clone)]
struct HistogramData {
    values: Vec<f64>,
    stats: HistogramStats,
}

impl HistogramData {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            stats: HistogramStats::new(),
        }
    }

    fn record(&mut self, value: f64) {
        self.values.push(value);
        self.stats.count += 1;
        self.stats.sum += value;
        self.stats.min = self.stats.min.min(value);
        self.stats.max = self.stats.max.max(value);
        self.stats.mean = self.stats.sum / self.stats.count as f64;
        self.update_percentiles();
    }

    fn update_percentiles(&mut self) {
        if self.values.is_empty() {
            return;
        }

        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let len = sorted.len();
        self.stats.percentiles.p50 = Self::percentile(&sorted, len, 0.50);
        self.stats.percentiles.p90 = Self::percentile(&sorted, len, 0.90);
        self.stats.percentiles.p95 = Self::percentile(&sorted, len, 0.95);
        self.stats.percentiles.p99 = Self::percentile(&sorted, len, 0.99);
    }

    fn percentile(sorted: &[f64], len: usize, p: f64) -> f64 {
        if len == 0 {
            return 0.0;
        }
        let idx = ((len as f64 * p) as usize).min(len - 1);
        sorted[idx]
    }

    fn stats(&self) -> HistogramStats {
        self.stats.clone()
    }
}

/// Snapshot of all metrics at a point in time
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub counters: HashMap<String, u64>,
    pub gauges: HashMap<String, f64>,
    pub histograms: HashMap<String, HistogramStats>,
}

/// Local in-memory metrics implementation.
///
/// This is the default metrics backend that stores all metrics in memory.
/// Suitable for development, testing, and single-instance deployments.
pub struct LocalMetrics {
    counters: RwLock<HashMap<String, u64>>,
    gauges: RwLock<HashMap<String, f64>>,
    histograms: RwLock<HashMap<String, HistogramData>>,
}

impl LocalMetrics {
    /// Create a new local metrics instance
    pub fn new() -> Self {
        Self {
            counters: RwLock::new(HashMap::new()),
            gauges: RwLock::new(HashMap::new()),
            histograms: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for LocalMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetricsBackend for LocalMetrics {
    async fn increment_counter(&self, name: &str, value: u64) {
        let mut counters = self.counters.write().await;
        *counters.entry(name.to_string()).or_insert(0) += value;
    }

    async fn set_gauge(&self, name: &str, value: f64) {
        let mut gauges = self.gauges.write().await;
        gauges.insert(name.to_string(), value);
    }

    async fn record_histogram(&self, name: &str, value: f64) {
        let mut histograms = self.histograms.write().await;
        histograms
            .entry(name.to_string())
            .or_insert_with(HistogramData::new)
            .record(value);
    }

    async fn get_counter(&self, name: &str) -> Option<u64> {
        let counters = self.counters.read().await;
        counters.get(name).copied()
    }

    async fn get_gauge(&self, name: &str) -> Option<f64> {
        let gauges = self.gauges.read().await;
        gauges.get(name).copied()
    }

    async fn get_histogram_stats(&self, name: &str) -> Option<HistogramStats> {
        let histograms = self.histograms.read().await;
        histograms.get(name).map(|h| h.stats())
    }

    async fn reset(&self) {
        let mut counters = self.counters.write().await;
        let mut gauges = self.gauges.write().await;
        let mut histograms = self.histograms.write().await;
        counters.clear();
        gauges.clear();
        histograms.clear();
    }

    async fn snapshot(&self) -> MetricsSnapshot {
        let counters = self.counters.read().await;
        let gauges = self.gauges.read().await;
        let histograms = self.histograms.read().await;

        MetricsSnapshot {
            counters: counters.clone(),
            gauges: gauges.clone(),
            histograms: histograms
                .iter()
                .map(|(k, v)| (k.clone(), v.stats()))
                .collect(),
        }
    }
}

/// Predefined metric names for queue observability
pub mod metric_names {
    /// Counter: Total commands submitted
    pub const COMMANDS_SUBMITTED: &str = "lane.commands.submitted";
    /// Counter: Total commands completed successfully
    pub const COMMANDS_COMPLETED: &str = "lane.commands.completed";
    /// Counter: Total commands failed
    pub const COMMANDS_FAILED: &str = "lane.commands.failed";
    /// Counter: Total commands timed out
    pub const COMMANDS_TIMEOUT: &str = "lane.commands.timeout";
    /// Counter: Total commands retried
    pub const COMMANDS_RETRIED: &str = "lane.commands.retried";
    /// Counter: Total commands sent to DLQ
    pub const COMMANDS_DEAD_LETTERED: &str = "lane.commands.dead_lettered";

    /// Gauge: Current queue depth (pending commands)
    pub const QUEUE_DEPTH: &str = "lane.queue.depth";
    /// Gauge: Current active commands
    pub const QUEUE_ACTIVE: &str = "lane.queue.active";

    /// Histogram: Command execution latency (ms)
    pub const COMMAND_LATENCY: &str = "lane.command.latency_ms";
    /// Histogram: Command wait time in queue (ms)
    pub const COMMAND_WAIT_TIME: &str = "lane.command.wait_time_ms";
}

/// Queue metrics collector that wraps a metrics backend
/// and provides convenient methods for queue-specific metrics.
pub struct QueueMetrics {
    backend: Arc<dyn MetricsBackend>,
}

impl QueueMetrics {
    /// Create a new queue metrics collector with the given backend
    pub fn new(backend: Arc<dyn MetricsBackend>) -> Self {
        Self { backend }
    }

    /// Create a new queue metrics collector with local in-memory backend
    pub fn local() -> Self {
        Self {
            backend: Arc::new(LocalMetrics::new()),
        }
    }

    /// Get the underlying metrics backend
    pub fn backend(&self) -> &Arc<dyn MetricsBackend> {
        &self.backend
    }

    /// Record a command submission
    pub async fn record_submit(&self, lane_id: &str) {
        self.backend
            .increment_counter(metric_names::COMMANDS_SUBMITTED, 1)
            .await;
        self.backend
            .increment_counter(
                &format!("{}.{}", metric_names::COMMANDS_SUBMITTED, lane_id),
                1,
            )
            .await;
    }

    /// Record a command completion
    pub async fn record_complete(&self, lane_id: &str, latency_ms: f64) {
        self.backend
            .increment_counter(metric_names::COMMANDS_COMPLETED, 1)
            .await;
        self.backend
            .increment_counter(
                &format!("{}.{}", metric_names::COMMANDS_COMPLETED, lane_id),
                1,
            )
            .await;
        self.backend
            .record_histogram(metric_names::COMMAND_LATENCY, latency_ms)
            .await;
        self.backend
            .record_histogram(
                &format!("{}.{}", metric_names::COMMAND_LATENCY, lane_id),
                latency_ms,
            )
            .await;
    }

    /// Record a command failure
    pub async fn record_failure(&self, lane_id: &str) {
        self.backend
            .increment_counter(metric_names::COMMANDS_FAILED, 1)
            .await;
        self.backend
            .increment_counter(&format!("{}.{}", metric_names::COMMANDS_FAILED, lane_id), 1)
            .await;
    }

    /// Record a command timeout
    pub async fn record_timeout(&self, lane_id: &str) {
        self.backend
            .increment_counter(metric_names::COMMANDS_TIMEOUT, 1)
            .await;
        self.backend
            .increment_counter(
                &format!("{}.{}", metric_names::COMMANDS_TIMEOUT, lane_id),
                1,
            )
            .await;
    }

    /// Record a command retry
    pub async fn record_retry(&self, lane_id: &str) {
        self.backend
            .increment_counter(metric_names::COMMANDS_RETRIED, 1)
            .await;
        self.backend
            .increment_counter(
                &format!("{}.{}", metric_names::COMMANDS_RETRIED, lane_id),
                1,
            )
            .await;
    }

    /// Record a command sent to dead letter queue
    pub async fn record_dead_letter(&self, lane_id: &str) {
        self.backend
            .increment_counter(metric_names::COMMANDS_DEAD_LETTERED, 1)
            .await;
        self.backend
            .increment_counter(
                &format!("{}.{}", metric_names::COMMANDS_DEAD_LETTERED, lane_id),
                1,
            )
            .await;
    }

    /// Update queue depth gauge
    pub async fn set_queue_depth(&self, lane_id: &str, depth: usize) {
        self.backend
            .set_gauge(
                &format!("{}.{}", metric_names::QUEUE_DEPTH, lane_id),
                depth as f64,
            )
            .await;
    }

    /// Update active commands gauge
    pub async fn set_active_commands(&self, lane_id: &str, active: usize) {
        self.backend
            .set_gauge(
                &format!("{}.{}", metric_names::QUEUE_ACTIVE, lane_id),
                active as f64,
            )
            .await;
    }

    /// Record command wait time in queue
    pub async fn record_wait_time(&self, lane_id: &str, wait_time_ms: f64) {
        self.backend
            .record_histogram(metric_names::COMMAND_WAIT_TIME, wait_time_ms)
            .await;
        self.backend
            .record_histogram(
                &format!("{}.{}", metric_names::COMMAND_WAIT_TIME, lane_id),
                wait_time_ms,
            )
            .await;
    }

    /// Get a snapshot of all metrics
    pub async fn snapshot(&self) -> MetricsSnapshot {
        self.backend.snapshot().await
    }

    /// Reset all metrics
    pub async fn reset(&self) {
        self.backend.reset().await;
    }
}

impl Clone for QueueMetrics {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_metrics_counter() {
        let metrics = LocalMetrics::new();

        assert_eq!(metrics.get_counter("test.counter").await, None);

        metrics.increment_counter("test.counter", 1).await;
        assert_eq!(metrics.get_counter("test.counter").await, Some(1));

        metrics.increment_counter("test.counter", 5).await;
        assert_eq!(metrics.get_counter("test.counter").await, Some(6));
    }

    #[tokio::test]
    async fn test_local_metrics_gauge() {
        let metrics = LocalMetrics::new();

        assert_eq!(metrics.get_gauge("test.gauge").await, None);

        metrics.set_gauge("test.gauge", 42.5).await;
        assert_eq!(metrics.get_gauge("test.gauge").await, Some(42.5));

        metrics.set_gauge("test.gauge", 100.0).await;
        assert_eq!(metrics.get_gauge("test.gauge").await, Some(100.0));
    }

    #[tokio::test]
    async fn test_local_metrics_histogram() {
        let metrics = LocalMetrics::new();

        assert!(metrics
            .get_histogram_stats("test.histogram")
            .await
            .is_none());

        metrics.record_histogram("test.histogram", 10.0).await;
        metrics.record_histogram("test.histogram", 20.0).await;
        metrics.record_histogram("test.histogram", 30.0).await;

        let stats = metrics.get_histogram_stats("test.histogram").await.unwrap();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.sum, 60.0);
        assert_eq!(stats.min, 10.0);
        assert_eq!(stats.max, 30.0);
        assert_eq!(stats.mean, 20.0);
    }

    #[tokio::test]
    async fn test_local_metrics_histogram_percentiles() {
        let metrics = LocalMetrics::new();

        // Record 100 values from 1 to 100
        for i in 1..=100 {
            metrics.record_histogram("test.histogram", i as f64).await;
        }

        let stats = metrics.get_histogram_stats("test.histogram").await.unwrap();
        assert_eq!(stats.count, 100);
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 100.0);

        // Check percentiles (approximate due to discrete values)
        assert!(stats.percentiles.p50 >= 49.0 && stats.percentiles.p50 <= 51.0);
        assert!(stats.percentiles.p90 >= 89.0 && stats.percentiles.p90 <= 91.0);
        assert!(stats.percentiles.p95 >= 94.0 && stats.percentiles.p95 <= 96.0);
        assert!(stats.percentiles.p99 >= 98.0 && stats.percentiles.p99 <= 100.0);
    }

    #[tokio::test]
    async fn test_local_metrics_reset() {
        let metrics = LocalMetrics::new();

        metrics.increment_counter("test.counter", 10).await;
        metrics.set_gauge("test.gauge", 50.0).await;
        metrics.record_histogram("test.histogram", 100.0).await;

        metrics.reset().await;

        assert_eq!(metrics.get_counter("test.counter").await, None);
        assert_eq!(metrics.get_gauge("test.gauge").await, None);
        assert!(metrics
            .get_histogram_stats("test.histogram")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_local_metrics_snapshot() {
        let metrics = LocalMetrics::new();

        metrics.increment_counter("counter1", 5).await;
        metrics.increment_counter("counter2", 10).await;
        metrics.set_gauge("gauge1", 42.0).await;
        metrics.record_histogram("histogram1", 100.0).await;

        let snapshot = metrics.snapshot().await;

        assert_eq!(snapshot.counters.get("counter1"), Some(&5));
        assert_eq!(snapshot.counters.get("counter2"), Some(&10));
        assert_eq!(snapshot.gauges.get("gauge1"), Some(&42.0));
        assert!(snapshot.histograms.contains_key("histogram1"));
    }

    #[tokio::test]
    async fn test_queue_metrics_record_submit() {
        let metrics = QueueMetrics::local();

        metrics.record_submit("query").await;
        metrics.record_submit("query").await;
        metrics.record_submit("system").await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_SUBMITTED),
            Some(&3)
        );
        assert_eq!(
            snapshot
                .counters
                .get(&format!("{}.query", metric_names::COMMANDS_SUBMITTED)),
            Some(&2)
        );
        assert_eq!(
            snapshot
                .counters
                .get(&format!("{}.system", metric_names::COMMANDS_SUBMITTED)),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_record_complete() {
        let metrics = QueueMetrics::local();

        metrics.record_complete("query", 50.0).await;
        metrics.record_complete("query", 100.0).await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_COMPLETED),
            Some(&2)
        );

        let latency_stats = snapshot
            .histograms
            .get(metric_names::COMMAND_LATENCY)
            .unwrap();
        assert_eq!(latency_stats.count, 2);
        assert_eq!(latency_stats.mean, 75.0);
    }

    #[tokio::test]
    async fn test_queue_metrics_record_failure() {
        let metrics = QueueMetrics::local();

        metrics.record_failure("query").await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_FAILED),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_record_timeout() {
        let metrics = QueueMetrics::local();

        metrics.record_timeout("query").await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_TIMEOUT),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_record_retry() {
        let metrics = QueueMetrics::local();

        metrics.record_retry("query").await;
        metrics.record_retry("query").await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_RETRIED),
            Some(&2)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_record_dead_letter() {
        let metrics = QueueMetrics::local();

        metrics.record_dead_letter("query").await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_DEAD_LETTERED),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_set_queue_depth() {
        let metrics = QueueMetrics::local();

        metrics.set_queue_depth("query", 10).await;
        metrics.set_queue_depth("system", 5).await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot
                .gauges
                .get(&format!("{}.query", metric_names::QUEUE_DEPTH)),
            Some(&10.0)
        );
        assert_eq!(
            snapshot
                .gauges
                .get(&format!("{}.system", metric_names::QUEUE_DEPTH)),
            Some(&5.0)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_set_active_commands() {
        let metrics = QueueMetrics::local();

        metrics.set_active_commands("query", 3).await;

        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot
                .gauges
                .get(&format!("{}.query", metric_names::QUEUE_ACTIVE)),
            Some(&3.0)
        );
    }

    #[tokio::test]
    async fn test_queue_metrics_record_wait_time() {
        let metrics = QueueMetrics::local();

        metrics.record_wait_time("query", 25.0).await;
        metrics.record_wait_time("query", 75.0).await;

        let snapshot = metrics.snapshot().await;
        let wait_stats = snapshot
            .histograms
            .get(metric_names::COMMAND_WAIT_TIME)
            .unwrap();
        assert_eq!(wait_stats.count, 2);
        assert_eq!(wait_stats.mean, 50.0);
    }

    #[tokio::test]
    async fn test_queue_metrics_clone() {
        let metrics = QueueMetrics::local();
        metrics.record_submit("query").await;

        let cloned = metrics.clone();
        cloned.record_submit("query").await;

        // Both should share the same backend
        let snapshot = metrics.snapshot().await;
        assert_eq!(
            snapshot.counters.get(metric_names::COMMANDS_SUBMITTED),
            Some(&2)
        );
    }

    #[tokio::test]
    async fn test_histogram_stats_default() {
        let stats = HistogramStats::default();
        assert_eq!(stats.count, 0);
        assert_eq!(stats.sum, 0.0);
        assert_eq!(stats.mean, 0.0);
    }

    #[test]
    fn test_histogram_percentiles_default() {
        let percentiles = HistogramPercentiles::default();
        assert_eq!(percentiles.p50, 0.0);
        assert_eq!(percentiles.p90, 0.0);
        assert_eq!(percentiles.p95, 0.0);
        assert_eq!(percentiles.p99, 0.0);
    }

    #[test]
    fn test_metrics_snapshot_default() {
        let snapshot = MetricsSnapshot::default();
        assert!(snapshot.counters.is_empty());
        assert!(snapshot.gauges.is_empty());
        assert!(snapshot.histograms.is_empty());
    }
}
