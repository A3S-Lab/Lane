//! Alert system for queue monitoring and notifications.
//!
//! This module provides configurable alerts for queue depth, latency,
//! and other metrics. Alerts can trigger callbacks for notifications.

use std::sync::Arc;
use tokio::sync::RwLock;

/// Alert severity level
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertLevel {
    /// Informational alert
    Info,
    /// Warning alert
    Warning,
    /// Critical alert
    Critical,
}

/// Alert condition that was triggered
#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    /// Alert level
    pub level: AlertLevel,
    /// Lane ID that triggered the alert
    pub lane_id: String,
    /// Alert message
    pub message: String,
    /// Current value that triggered the alert
    pub current_value: f64,
    /// Threshold that was exceeded
    pub threshold: f64,
}

/// Callback function type for alert notifications
pub type AlertCallback = Arc<dyn Fn(Alert) + Send + Sync>;

/// Configuration for queue depth alerts
#[derive(Debug, Clone, PartialEq)]
pub struct QueueDepthAlertConfig {
    /// Warning threshold (queue depth)
    pub warning_threshold: usize,
    /// Critical threshold (queue depth)
    pub critical_threshold: usize,
    /// Enable alerts
    pub enabled: bool,
}

impl QueueDepthAlertConfig {
    /// Create a new queue depth alert configuration
    pub fn new(warning_threshold: usize, critical_threshold: usize) -> Self {
        Self {
            warning_threshold,
            critical_threshold,
            enabled: true,
        }
    }

    /// Disable alerts
    pub fn disabled() -> Self {
        Self {
            warning_threshold: usize::MAX,
            critical_threshold: usize::MAX,
            enabled: false,
        }
    }
}

impl Default for QueueDepthAlertConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Configuration for latency alerts
#[derive(Debug, Clone, PartialEq)]
pub struct LatencyAlertConfig {
    /// Warning threshold (milliseconds)
    pub warning_threshold_ms: f64,
    /// Critical threshold (milliseconds)
    pub critical_threshold_ms: f64,
    /// Enable alerts
    pub enabled: bool,
}

impl LatencyAlertConfig {
    /// Create a new latency alert configuration
    pub fn new(warning_threshold_ms: f64, critical_threshold_ms: f64) -> Self {
        Self {
            warning_threshold_ms,
            critical_threshold_ms,
            enabled: true,
        }
    }

    /// Disable alerts
    pub fn disabled() -> Self {
        Self {
            warning_threshold_ms: f64::MAX,
            critical_threshold_ms: f64::MAX,
            enabled: false,
        }
    }
}

impl Default for LatencyAlertConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Alert manager for monitoring queue metrics and triggering alerts
pub struct AlertManager {
    queue_depth_config: RwLock<QueueDepthAlertConfig>,
    latency_config: RwLock<LatencyAlertConfig>,
    callbacks: RwLock<Vec<AlertCallback>>,
}

impl AlertManager {
    /// Create a new alert manager
    pub fn new() -> Self {
        Self {
            queue_depth_config: RwLock::new(QueueDepthAlertConfig::default()),
            latency_config: RwLock::new(LatencyAlertConfig::default()),
            callbacks: RwLock::new(Vec::new()),
        }
    }

    /// Create a new alert manager with queue depth alerts enabled
    pub fn with_queue_depth_alerts(
        warning_threshold: usize,
        critical_threshold: usize,
    ) -> Self {
        Self {
            queue_depth_config: RwLock::new(QueueDepthAlertConfig::new(
                warning_threshold,
                critical_threshold,
            )),
            latency_config: RwLock::new(LatencyAlertConfig::default()),
            callbacks: RwLock::new(Vec::new()),
        }
    }

    /// Create a new alert manager with latency alerts enabled
    pub fn with_latency_alerts(
        warning_threshold_ms: f64,
        critical_threshold_ms: f64,
    ) -> Self {
        Self {
            queue_depth_config: RwLock::new(QueueDepthAlertConfig::default()),
            latency_config: RwLock::new(LatencyAlertConfig::new(
                warning_threshold_ms,
                critical_threshold_ms,
            )),
            callbacks: RwLock::new(Vec::new()),
        }
    }

    /// Set queue depth alert configuration
    pub async fn set_queue_depth_config(&self, config: QueueDepthAlertConfig) {
        let mut queue_depth_config = self.queue_depth_config.write().await;
        *queue_depth_config = config;
    }

    /// Set latency alert configuration
    pub async fn set_latency_config(&self, config: LatencyAlertConfig) {
        let mut latency_config = self.latency_config.write().await;
        *latency_config = config;
    }

    /// Add an alert callback
    pub async fn add_callback<F>(&self, callback: F)
    where
        F: Fn(Alert) + Send + Sync + 'static,
    {
        let mut callbacks = self.callbacks.write().await;
        callbacks.push(Arc::new(callback));
    }

    /// Check queue depth and trigger alerts if thresholds are exceeded
    pub async fn check_queue_depth(&self, lane_id: &str, depth: usize) {
        let config = self.queue_depth_config.read().await;
        if !config.enabled {
            return;
        }

        let alert = if depth >= config.critical_threshold {
            Some(Alert {
                level: AlertLevel::Critical,
                lane_id: lane_id.to_string(),
                message: format!(
                    "Queue depth critical: {} (threshold: {})",
                    depth, config.critical_threshold
                ),
                current_value: depth as f64,
                threshold: config.critical_threshold as f64,
            })
        } else if depth >= config.warning_threshold {
            Some(Alert {
                level: AlertLevel::Warning,
                lane_id: lane_id.to_string(),
                message: format!(
                    "Queue depth warning: {} (threshold: {})",
                    depth, config.warning_threshold
                ),
                current_value: depth as f64,
                threshold: config.warning_threshold as f64,
            })
        } else {
            None
        };

        if let Some(alert) = alert {
            self.trigger_alert(alert).await;
        }
    }

    /// Check latency and trigger alerts if thresholds are exceeded
    pub async fn check_latency(&self, lane_id: &str, latency_ms: f64) {
        let config = self.latency_config.read().await;
        if !config.enabled {
            return;
        }

        let alert = if latency_ms >= config.critical_threshold_ms {
            Some(Alert {
                level: AlertLevel::Critical,
                lane_id: lane_id.to_string(),
                message: format!(
                    "Latency critical: {:.2}ms (threshold: {:.2}ms)",
                    latency_ms, config.critical_threshold_ms
                ),
                current_value: latency_ms,
                threshold: config.critical_threshold_ms,
            })
        } else if latency_ms >= config.warning_threshold_ms {
            Some(Alert {
                level: AlertLevel::Warning,
                lane_id: lane_id.to_string(),
                message: format!(
                    "Latency warning: {:.2}ms (threshold: {:.2}ms)",
                    latency_ms, config.warning_threshold_ms
                ),
                current_value: latency_ms,
                threshold: config.warning_threshold_ms,
            })
        } else {
            None
        };

        if let Some(alert) = alert {
            self.trigger_alert(alert).await;
        }
    }

    /// Trigger an alert by calling all registered callbacks
    async fn trigger_alert(&self, alert: Alert) {
        let callbacks = self.callbacks.read().await;
        for callback in callbacks.iter() {
            callback(alert.clone());
        }
    }

    /// Get current queue depth alert configuration
    pub async fn queue_depth_config(&self) -> QueueDepthAlertConfig {
        self.queue_depth_config.read().await.clone()
    }

    /// Get current latency alert configuration
    pub async fn latency_config(&self) -> LatencyAlertConfig {
        self.latency_config.read().await.clone()
    }
}

impl Default for AlertManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for AlertManager {
    fn clone(&self) -> Self {
        // Create a new AlertManager with default configs
        // Note: callbacks are not cloned as they can't be easily cloned
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn test_queue_depth_alert_config() {
        let config = QueueDepthAlertConfig::new(100, 200);
        assert_eq!(config.warning_threshold, 100);
        assert_eq!(config.critical_threshold, 200);
        assert!(config.enabled);

        let disabled = QueueDepthAlertConfig::disabled();
        assert!(!disabled.enabled);
    }

    #[tokio::test]
    async fn test_latency_alert_config() {
        let config = LatencyAlertConfig::new(100.0, 500.0);
        assert_eq!(config.warning_threshold_ms, 100.0);
        assert_eq!(config.critical_threshold_ms, 500.0);
        assert!(config.enabled);

        let disabled = LatencyAlertConfig::disabled();
        assert!(!disabled.enabled);
    }

    #[tokio::test]
    async fn test_alert_manager_new() {
        let manager = AlertManager::new();
        let queue_config = manager.queue_depth_config().await;
        let latency_config = manager.latency_config().await;

        assert!(!queue_config.enabled);
        assert!(!latency_config.enabled);
    }

    #[tokio::test]
    async fn test_alert_manager_with_queue_depth_alerts() {
        let manager = AlertManager::with_queue_depth_alerts(50, 100);
        let config = manager.queue_depth_config().await;

        assert_eq!(config.warning_threshold, 50);
        assert_eq!(config.critical_threshold, 100);
        assert!(config.enabled);
    }

    #[tokio::test]
    async fn test_alert_manager_with_latency_alerts() {
        let manager = AlertManager::with_latency_alerts(100.0, 500.0);
        let config = manager.latency_config().await;

        assert_eq!(config.warning_threshold_ms, 100.0);
        assert_eq!(config.critical_threshold_ms, 500.0);
        assert!(config.enabled);
    }

    #[tokio::test]
    async fn test_alert_manager_set_configs() {
        let manager = AlertManager::new();

        manager
            .set_queue_depth_config(QueueDepthAlertConfig::new(10, 20))
            .await;
        let queue_config = manager.queue_depth_config().await;
        assert_eq!(queue_config.warning_threshold, 10);
        assert_eq!(queue_config.critical_threshold, 20);

        manager
            .set_latency_config(LatencyAlertConfig::new(50.0, 100.0))
            .await;
        let latency_config = manager.latency_config().await;
        assert_eq!(latency_config.warning_threshold_ms, 50.0);
        assert_eq!(latency_config.critical_threshold_ms, 100.0);
    }

    #[tokio::test]
    async fn test_check_queue_depth_no_alert() {
        let manager = AlertManager::with_queue_depth_alerts(100, 200);
        let alert_count = Arc::new(AtomicUsize::new(0));
        let alert_count_clone = Arc::clone(&alert_count);

        manager
            .add_callback(move |_alert| {
                alert_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        manager.check_queue_depth("query", 50).await;

        // No alert should be triggered
        assert_eq!(alert_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_check_queue_depth_warning() {
        let manager = AlertManager::with_queue_depth_alerts(100, 200);
        let alert_level = Arc::new(RwLock::new(None));
        let alert_level_clone = Arc::clone(&alert_level);

        manager
            .add_callback(move |alert| {
                let alert_level = Arc::clone(&alert_level_clone);
                tokio::spawn(async move {
                    let mut level = alert_level.write().await;
                    *level = Some(alert.level);
                });
            })
            .await;

        manager.check_queue_depth("query", 150).await;

        // Give callback time to execute
        tokio::time::sleep(Duration::from_millis(10)).await;

        let level = alert_level.read().await;
        assert_eq!(*level, Some(AlertLevel::Warning));
    }

    #[tokio::test]
    async fn test_check_queue_depth_critical() {
        let manager = AlertManager::with_queue_depth_alerts(100, 200);
        let alert_level = Arc::new(RwLock::new(None));
        let alert_level_clone = Arc::clone(&alert_level);

        manager
            .add_callback(move |alert| {
                let alert_level = Arc::clone(&alert_level_clone);
                tokio::spawn(async move {
                    let mut level = alert_level.write().await;
                    *level = Some(alert.level);
                });
            })
            .await;

        manager.check_queue_depth("query", 250).await;

        // Give callback time to execute
        tokio::time::sleep(Duration::from_millis(10)).await;

        let level = alert_level.read().await;
        assert_eq!(*level, Some(AlertLevel::Critical));
    }

    #[tokio::test]
    async fn test_check_queue_depth_disabled() {
        let manager = AlertManager::new();
        let alert_count = Arc::new(AtomicUsize::new(0));
        let alert_count_clone = Arc::clone(&alert_count);

        manager
            .add_callback(move |_alert| {
                alert_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        manager.check_queue_depth("query", 1000).await;

        // No alert should be triggered when disabled
        assert_eq!(alert_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_check_latency_no_alert() {
        let manager = AlertManager::with_latency_alerts(100.0, 500.0);
        let alert_count = Arc::new(AtomicUsize::new(0));
        let alert_count_clone = Arc::clone(&alert_count);

        manager
            .add_callback(move |_alert| {
                alert_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        manager.check_latency("query", 50.0).await;

        // No alert should be triggered
        assert_eq!(alert_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_check_latency_warning() {
        let manager = AlertManager::with_latency_alerts(100.0, 500.0);
        let alert_level = Arc::new(RwLock::new(None));
        let alert_level_clone = Arc::clone(&alert_level);

        manager
            .add_callback(move |alert| {
                let alert_level = Arc::clone(&alert_level_clone);
                tokio::spawn(async move {
                    let mut level = alert_level.write().await;
                    *level = Some(alert.level);
                });
            })
            .await;

        manager.check_latency("query", 250.0).await;

        // Give callback time to execute
        tokio::time::sleep(Duration::from_millis(10)).await;

        let level = alert_level.read().await;
        assert_eq!(*level, Some(AlertLevel::Warning));
    }

    #[tokio::test]
    async fn test_check_latency_critical() {
        let manager = AlertManager::with_latency_alerts(100.0, 500.0);
        let alert_level = Arc::new(RwLock::new(None));
        let alert_level_clone = Arc::clone(&alert_level);

        manager
            .add_callback(move |alert| {
                let alert_level = Arc::clone(&alert_level_clone);
                tokio::spawn(async move {
                    let mut level = alert_level.write().await;
                    *level = Some(alert.level);
                });
            })
            .await;

        manager.check_latency("query", 600.0).await;

        // Give callback time to execute
        tokio::time::sleep(Duration::from_millis(10)).await;

        let level = alert_level.read().await;
        assert_eq!(*level, Some(AlertLevel::Critical));
    }

    #[tokio::test]
    async fn test_check_latency_disabled() {
        let manager = AlertManager::new();
        let alert_count = Arc::new(AtomicUsize::new(0));
        let alert_count_clone = Arc::clone(&alert_count);

        manager
            .add_callback(move |_alert| {
                alert_count_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        manager.check_latency("query", 10000.0).await;

        // No alert should be triggered when disabled
        assert_eq!(alert_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_multiple_callbacks() {
        let manager = AlertManager::with_queue_depth_alerts(100, 200);
        let count1 = Arc::new(AtomicUsize::new(0));
        let count2 = Arc::new(AtomicUsize::new(0));

        let count1_clone = Arc::clone(&count1);
        manager
            .add_callback(move |_alert| {
                count1_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        let count2_clone = Arc::clone(&count2);
        manager
            .add_callback(move |_alert| {
                count2_clone.fetch_add(1, Ordering::SeqCst);
            })
            .await;

        manager.check_queue_depth("query", 150).await;

        // Both callbacks should be triggered
        assert_eq!(count1.load(Ordering::SeqCst), 1);
        assert_eq!(count2.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_alert_level_ordering() {
        assert!(AlertLevel::Info < AlertLevel::Warning);
        assert!(AlertLevel::Warning < AlertLevel::Critical);
    }

    #[tokio::test]
    async fn test_alert_manager_default() {
        let manager = AlertManager::default();
        let queue_config = manager.queue_depth_config().await;
        assert!(!queue_config.enabled);
    }

    #[tokio::test]
    async fn test_queue_depth_alert_config_default() {
        let config = QueueDepthAlertConfig::default();
        assert!(!config.enabled);
    }

    #[tokio::test]
    async fn test_latency_alert_config_default() {
        let config = LatencyAlertConfig::default();
        assert!(!config.enabled);
    }
}
