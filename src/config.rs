//! Lane configuration types

use crate::boost::PriorityBoostConfig;
use crate::ratelimit::RateLimitConfig;
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
    /// Rate limit configuration
    #[serde(skip)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Priority boost configuration
    #[serde(skip)]
    pub priority_boost: Option<PriorityBoostConfig>,
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
            rate_limit: None,
            priority_boost: None,
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
            rate_limit: None,
            priority_boost: None,
        }
    }

    /// Set timeout (builder pattern)
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    /// Set retry policy (builder pattern)
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Set rate limit (builder pattern)
    pub fn with_rate_limit(mut self, rate_limit: RateLimitConfig) -> Self {
        self.rate_limit = Some(rate_limit);
        self
    }

    /// Set priority boost (builder pattern)
    pub fn with_priority_boost(mut self, priority_boost: PriorityBoostConfig) -> Self {
        self.priority_boost = Some(priority_boost);
        self
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
        let config = LaneConfig::new(2, 8).with_timeout(Duration::from_secs(30));
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert_eq!(config.default_timeout, Some(Duration::from_secs(30)));
        assert_eq!(config.retry_policy, RetryPolicy::none());
    }

    #[test]
    fn test_lane_config_with_retry() {
        let retry = RetryPolicy::exponential(3);
        let config = LaneConfig::new(2, 8).with_retry_policy(retry.clone());
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert!(config.default_timeout.is_none());
        assert_eq!(config.retry_policy, retry);
    }

    #[test]
    fn test_lane_config_with_timeout_and_retry() {
        let retry = RetryPolicy::fixed(5, Duration::from_secs(1));
        let config = LaneConfig::new(2, 8)
            .with_timeout(Duration::from_secs(30))
            .with_retry_policy(retry.clone());
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
        assert_eq!(config.default_timeout, Some(Duration::from_secs(30)));
        assert_eq!(config.retry_policy, retry);
    }

    #[test]
    fn test_lane_config_with_rate_limit() {
        let rate_limit = RateLimitConfig::per_second(100);
        let config = LaneConfig::new(2, 8).with_rate_limit(rate_limit.clone());
        assert!(config.rate_limit.is_some());
        assert_eq!(config.rate_limit.unwrap().max_commands, 100);
    }

    #[test]
    fn test_lane_config_with_priority_boost() {
        let boost = PriorityBoostConfig::standard(Duration::from_secs(60));
        let config = LaneConfig::new(2, 8).with_priority_boost(boost);
        assert!(config.priority_boost.is_some());
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
