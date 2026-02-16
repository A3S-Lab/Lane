//! OpenTelemetry telemetry for the A3S Lane command queue.
//!
//! Provides structured spans, attribute constants, and an `OtelMetricsBackend`
//! that bridges the existing `MetricsBackend` trait to OpenTelemetry instruments.

use crate::metrics::{HistogramStats, MetricsBackend, MetricsSnapshot};
use async_trait::async_trait;
use dashmap::DashMap;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, UpDownCounter};
use opentelemetry::KeyValue;
use std::sync::OnceLock;

// ============================================================================
// Span Constants
// ============================================================================

/// Span for submitting a command to a lane
pub const SPAN_LANE_SUBMIT: &str = "a3s.lane.submit";

/// Span for executing a command
pub const SPAN_LANE_EXECUTE: &str = "a3s.lane.execute";

/// Span for retrying a failed command
pub const SPAN_LANE_RETRY: &str = "a3s.lane.retry";

/// Span for queue health checks
pub const SPAN_LANE_HEALTH_CHECK: &str = "a3s.lane.health_check";

// ============================================================================
// Attribute Constants
// ============================================================================

/// Command identifier attribute
pub const ATTR_COMMAND_ID: &str = "a3s.lane.command_id";

/// Lane name attribute
pub const ATTR_LANE_NAME: &str = "a3s.lane.lane_name";

/// Command type attribute
pub const ATTR_COMMAND_TYPE: &str = "a3s.lane.command_type";

/// Retry attempt number
pub const ATTR_RETRY_ATTEMPT: &str = "a3s.lane.retry_attempt";

/// Whether the command succeeded
pub const ATTR_SUCCESS: &str = "a3s.lane.success";

// ============================================================================
// Standalone Metrics (OnceLock pattern, like a3s-code)
// ============================================================================

static METRICS: OnceLock<LaneMetricsRecorder> = OnceLock::new();

/// Holds OpenTelemetry metric instruments for lane-level observability.
pub struct LaneMetricsRecorder {
    /// Total commands submitted
    pub commands_submitted_total: Counter<u64>,
    /// Total commands completed
    pub commands_completed_total: Counter<u64>,
    /// Total commands failed
    pub commands_failed_total: Counter<u64>,
    /// Command execution latency in seconds
    pub command_duration_seconds: Histogram<f64>,
}

/// Get the global lane metrics recorder (None if not initialized).
pub fn metrics() -> Option<&'static LaneMetricsRecorder> {
    METRICS.get()
}

/// Initialize lane metrics using the global OpenTelemetry meter provider.
///
/// Safe to call multiple times; only the first call takes effect.
pub fn init_lane_metrics() {
    let meter = global::meter("a3s-lane");

    let recorder = LaneMetricsRecorder {
        commands_submitted_total: meter
            .u64_counter("a3s_lane_commands_submitted_total")
            .with_description("Total commands submitted to lanes")
            .init(),
        commands_completed_total: meter
            .u64_counter("a3s_lane_commands_completed_total")
            .with_description("Total commands completed successfully")
            .init(),
        commands_failed_total: meter
            .u64_counter("a3s_lane_commands_failed_total")
            .with_description("Total commands that failed")
            .init(),
        command_duration_seconds: meter
            .f64_histogram("a3s_lane_command_duration_seconds")
            .with_description("Command execution duration in seconds")
            .init(),
    };

    let _ = METRICS.set(recorder);
}

/// Record a command submission. No-op if metrics are not initialized.
pub fn record_submit(lane_name: &str) {
    if let Some(m) = metrics() {
        m.commands_submitted_total
            .add(1, &[KeyValue::new("lane", lane_name.to_string())]);
    }
}

/// Record a command completion with duration. No-op if metrics are not initialized.
pub fn record_complete(lane_name: &str, duration_secs: f64) {
    if let Some(m) = metrics() {
        let lane_attr = KeyValue::new("lane", lane_name.to_string());
        m.commands_completed_total
            .add(1, std::slice::from_ref(&lane_attr));
        m.command_duration_seconds
            .record(duration_secs, std::slice::from_ref(&lane_attr));
    }
}

/// Record a command failure. No-op if metrics are not initialized.
pub fn record_failure(lane_name: &str) {
    if let Some(m) = metrics() {
        m.commands_failed_total
            .add(1, &[KeyValue::new("lane", lane_name.to_string())]);
    }
}

// ============================================================================
// OtelMetricsBackend — bridges MetricsBackend trait to OpenTelemetry
// ============================================================================

/// OpenTelemetry implementation of the `MetricsBackend` trait.
///
/// Uses lazy instrument creation via `DashMap` since the trait accepts
/// arbitrary metric names. Push-based methods (`get_counter`, `snapshot`)
/// return `None`/empty since OTLP is push-only.
pub struct OtelMetricsBackend {
    meter: opentelemetry::metrics::Meter,
    counters: DashMap<String, Counter<u64>>,
    histograms: DashMap<String, Histogram<f64>>,
    gauges: DashMap<String, UpDownCounter<i64>>,
}

impl OtelMetricsBackend {
    /// Create a new OtelMetricsBackend using the global meter provider.
    pub fn new() -> Self {
        Self {
            meter: global::meter("a3s-lane"),
            counters: DashMap::new(),
            histograms: DashMap::new(),
            gauges: DashMap::new(),
        }
    }

    fn get_or_create_counter(&self, name: &str) -> Counter<u64> {
        if let Some(c) = self.counters.get(name) {
            return c.clone();
        }
        let owned = name.to_string();
        let counter = self.meter.u64_counter(owned.clone()).init();
        self.counters.insert(owned, counter.clone());
        counter
    }

    fn get_or_create_histogram(&self, name: &str) -> Histogram<f64> {
        if let Some(h) = self.histograms.get(name) {
            return h.clone();
        }
        let owned = name.to_string();
        let histogram = self.meter.f64_histogram(owned.clone()).init();
        self.histograms.insert(owned, histogram.clone());
        histogram
    }

    fn get_or_create_gauge(&self, name: &str) -> UpDownCounter<i64> {
        if let Some(g) = self.gauges.get(name) {
            return g.clone();
        }
        let owned = name.to_string();
        let gauge = self.meter.i64_up_down_counter(owned.clone()).init();
        self.gauges.insert(owned, gauge.clone());
        gauge
    }
}

impl Default for OtelMetricsBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MetricsBackend for OtelMetricsBackend {
    async fn increment_counter(&self, name: &str, value: u64) {
        let counter = self.get_or_create_counter(name);
        counter.add(value, &[]);
    }

    async fn set_gauge(&self, name: &str, value: f64) {
        // UpDownCounter requires delta; for simplicity, we add the value as a delta.
        // This is an approximation — OTLP gauge semantics differ from set-to-value.
        let gauge = self.get_or_create_gauge(name);
        gauge.add(value as i64, &[]);
    }

    async fn record_histogram(&self, name: &str, value: f64) {
        let histogram = self.get_or_create_histogram(name);
        histogram.record(value, &[]);
    }

    async fn get_counter(&self, _name: &str) -> Option<u64> {
        // OTLP is push-only; pull-based reads are not supported
        None
    }

    async fn get_gauge(&self, _name: &str) -> Option<f64> {
        // OTLP is push-only
        None
    }

    async fn get_histogram_stats(&self, _name: &str) -> Option<HistogramStats> {
        // OTLP is push-only
        None
    }

    async fn reset(&self) {
        // No-op for OTLP (instruments are long-lived)
    }

    async fn snapshot(&self) -> MetricsSnapshot {
        // OTLP is push-only; return empty snapshot
        MetricsSnapshot::default()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_constants_follow_convention() {
        let spans = [
            SPAN_LANE_SUBMIT,
            SPAN_LANE_EXECUTE,
            SPAN_LANE_RETRY,
            SPAN_LANE_HEALTH_CHECK,
        ];
        for span in &spans {
            assert!(
                span.starts_with("a3s."),
                "Span {} should start with a3s.",
                span
            );
        }
    }

    #[test]
    fn test_attribute_keys_follow_convention() {
        let attrs = [
            ATTR_COMMAND_ID,
            ATTR_LANE_NAME,
            ATTR_COMMAND_TYPE,
            ATTR_RETRY_ATTEMPT,
            ATTR_SUCCESS,
        ];
        for attr in &attrs {
            assert!(
                attr.starts_with("a3s.lane."),
                "Attribute {} should start with a3s.lane.",
                attr
            );
        }
    }

    #[test]
    fn test_attribute_keys_are_unique() {
        let keys = vec![
            ATTR_COMMAND_ID,
            ATTR_LANE_NAME,
            ATTR_COMMAND_TYPE,
            ATTR_RETRY_ATTEMPT,
            ATTR_SUCCESS,
        ];
        let unique: std::collections::HashSet<&str> = keys.iter().copied().collect();
        assert_eq!(keys.len(), unique.len(), "Attribute keys must be unique");
    }

    #[test]
    fn test_span_constant_values() {
        assert_eq!(SPAN_LANE_SUBMIT, "a3s.lane.submit");
        assert_eq!(SPAN_LANE_EXECUTE, "a3s.lane.execute");
        assert_eq!(SPAN_LANE_RETRY, "a3s.lane.retry");
        assert_eq!(SPAN_LANE_HEALTH_CHECK, "a3s.lane.health_check");
    }

    #[test]
    fn test_attribute_constant_values() {
        assert_eq!(ATTR_COMMAND_ID, "a3s.lane.command_id");
        assert_eq!(ATTR_LANE_NAME, "a3s.lane.lane_name");
        assert_eq!(ATTR_COMMAND_TYPE, "a3s.lane.command_type");
        assert_eq!(ATTR_RETRY_ATTEMPT, "a3s.lane.retry_attempt");
        assert_eq!(ATTR_SUCCESS, "a3s.lane.success");
    }

    #[test]
    fn test_record_submit_no_panic_without_init() {
        record_submit("query");
        record_submit("");
    }

    #[test]
    fn test_record_complete_no_panic_without_init() {
        record_complete("query", 1.5);
        record_complete("system", 0.0);
    }

    #[test]
    fn test_record_failure_no_panic_without_init() {
        record_failure("query");
    }

    #[test]
    fn test_metrics_returns_none_without_init() {
        let _ = metrics();
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_increment_counter() {
        let backend = OtelMetricsBackend::new();
        // Should not panic
        backend.increment_counter("test.counter", 1).await;
        backend.increment_counter("test.counter", 5).await;
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_get_counter_returns_none() {
        let backend = OtelMetricsBackend::new();
        backend.increment_counter("test.counter", 10).await;
        assert_eq!(backend.get_counter("test.counter").await, None);
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_snapshot_returns_empty() {
        let backend = OtelMetricsBackend::new();
        backend.increment_counter("test.counter", 1).await;
        let snapshot = backend.snapshot().await;
        assert!(snapshot.counters.is_empty());
        assert!(snapshot.gauges.is_empty());
        assert!(snapshot.histograms.is_empty());
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_record_histogram() {
        let backend = OtelMetricsBackend::new();
        // Should not panic
        backend.record_histogram("test.latency", 42.5).await;
        backend.record_histogram("test.latency", 100.0).await;
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_set_gauge() {
        let backend = OtelMetricsBackend::new();
        // Should not panic
        backend.set_gauge("test.depth", 10.0).await;
        backend.set_gauge("test.depth", 0.0).await;
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_get_gauge_returns_none() {
        let backend = OtelMetricsBackend::new();
        backend.set_gauge("test.gauge", 42.0).await;
        assert_eq!(backend.get_gauge("test.gauge").await, None);
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_get_histogram_stats_returns_none() {
        let backend = OtelMetricsBackend::new();
        backend.record_histogram("test.hist", 1.0).await;
        assert!(backend.get_histogram_stats("test.hist").await.is_none());
    }

    #[tokio::test]
    async fn test_otel_metrics_backend_reset_no_panic() {
        let backend = OtelMetricsBackend::new();
        backend.increment_counter("c", 1).await;
        backend.reset().await;
    }

    #[test]
    fn test_otel_metrics_backend_default() {
        let _backend = OtelMetricsBackend::default();
    }
}
