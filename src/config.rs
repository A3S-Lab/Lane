//! Lane configuration types

use crate::retry::RetryPolicy;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Lane configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneConfig {
    /// Minimum concurrency (reserved slots)
    pub min_concurrency: usize,
    /// Maximum concurrency (capacity limit)
    pub max_concurrency: usize,
    /// Default timeout for commands in this lane
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "duration_serde"
    )]
    pub default_timeout: Option<Duration>,
    /// Retry policy for failed commands
    #[serde(default)]
    pub retry_policy: RetryPolicy,
}

mod duration_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(d) => serializer.serialize_u64(d.as_millis() as u64),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis: Option<u64> = Option::deserialize(deserializer)?;
        Ok(millis.map(Duration::from_millis))
    }
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            min_concurrency: 1,
            max_concurrency: 4,
            default_timeout: None,
            retry_policy: RetryPolicy::default(),
        }
    }
}

impl LaneConfig {
    /// Create a new lane configuration
    pub fn new(min_concurrency: usize, max_concurrency: usize) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
            default_timeout: None,
            retry_policy: RetryPolicy::default(),
        }
    }

    /// Create a new lane configuration with timeout
    pub fn with_timeout(
        min_concurrency: usize,
        max_concurrency: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
            default_timeout: Some(timeout),
            retry_policy: RetryPolicy::default(),
        }
    }

    /// Create a new lane configuration with retry policy
    pub fn with_retry(
        min_concurrency: usize,
        max_concurrency: usize,
        retry_policy: RetryPolicy,
    ) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
            default_timeout: None,
            retry_policy,
        }
    }

    /// Create a new lane configuration with timeout and retry policy
    pub fn with_timeout_and_retry(
        min_concurrency: usize,
        max_concurrency: usize,
        timeout: Duration,
        retry_policy: RetryPolicy,
    ) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
            default_timeout: Some(timeout),
            retry_policy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lane_config_default() {
        let config = LaneConfig::default();
        assert_eq!(config.min_concurrency, 1);
        assert_eq!(config.max_concurrency, 4);
        assert!(config.default_timeout.is_none());
    }

    #[test]
    fn test_lane_config_new() {
        let config = LaneConfig::new(2, 8);
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert!(config.default_timeout.is_none());
    }

    #[test]
    fn test_lane_config_with_timeout() {
        let config = LaneConfig::with_timeout(2, 8, Duration::from_secs(30));
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert_eq!(config.default_timeout, Some(Duration::from_secs(30)));
        assert_eq!(config.retry_policy, RetryPolicy::none());
    }

    #[test]
    fn test_lane_config_with_retry() {
        let retry = RetryPolicy::exponential(3);
        let config = LaneConfig::with_retry(2, 8, retry.clone());
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert!(config.default_timeout.is_none());
        assert_eq!(config.retry_policy, retry);
    }

    #[test]
    fn test_lane_config_with_timeout_and_retry() {
        let retry = RetryPolicy::fixed(5, Duration::from_secs(1));
        let config =
            LaneConfig::with_timeout_and_retry(2, 8, Duration::from_secs(30), retry.clone());
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert_eq!(config.default_timeout, Some(Duration::from_secs(30)));
        assert_eq!(config.retry_policy, retry);
    }

    #[test]
    fn test_lane_config_clone() {
        let config = LaneConfig::new(3, 16);
        let cloned = config.clone();
        assert_eq!(cloned.min_concurrency, 3);
        assert_eq!(cloned.max_concurrency, 16);
    }

    #[test]
    fn test_lane_config_serialization() {
        let config = LaneConfig::new(2, 10);
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"min_concurrency\":2"));
        assert!(json.contains("\"max_concurrency\":10"));

        let parsed: LaneConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn test_lane_config_debug() {
        let config = LaneConfig::new(1, 5);
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("LaneConfig"));
        assert!(debug_str.contains("min_concurrency"));
        assert!(debug_str.contains("max_concurrency"));
    }
}
