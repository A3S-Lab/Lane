//! Lane configuration types
//!
//! This module provides configuration types for lanes, including concurrency limits,
//! timeouts, retry policies, rate limiting, and priority boosting.
//!
//! # Configuration Options
//!
//! ## Concurrency Control
//! - `min_concurrency`: Reserved slots for this lane (guaranteed capacity)
//! - `max_concurrency`: Maximum concurrent commands allowed
//!
//! ## Reliability
//! - `default_timeout`: Optional timeout for commands in this lane
//! - `retry_policy`: Retry strategy for failed commands (exponential backoff, fixed delay, none)
//!
//! ## Scalability
//! - `rate_limit`: Optional rate limiting (token bucket or sliding window)
//! - `priority_boost`: Optional deadline-based priority adjustment
//!
//! # Example
//!
//! ```rust,ignore
//! use a3s_lane::{LaneConfig, RetryPolicy, RateLimitConfig};
//! use std::time::Duration;
//!
//! // Create a lane with timeout, retry, and rate limiting
//! let config = LaneConfig::new(1, 10)
//!     .with_timeout(Duration::from_secs(30))
//!     .with_retry_policy(RetryPolicy::exponential(3))
//!     .with_rate_limit(RateLimitConfig::per_second(100));
//! ```

use crate::boost::PriorityBoostConfig;
use crate::ratelimit::RateLimitConfig;
use crate::retry::RetryPolicy;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Lane configuration
///
/// Defines the behavior and limits for a single lane in the queue system.
///
/// # Fields
///
/// * `min_concurrency` - Minimum concurrency (reserved slots). Commands in this lane
///   are guaranteed at least this many concurrent execution slots.
/// * `max_concurrency` - Maximum concurrency (capacity limit). No more than this many
///   commands from this lane can execute concurrently.
/// * `default_timeout` - Optional timeout for commands. If set, commands that exceed
///   this duration will be cancelled and return a `Timeout` error.
/// * `retry_policy` - Retry strategy for failed commands. Defaults to no retries.
/// * `rate_limit` - Optional rate limiting configuration. If set, commands will be
///   throttled according to the specified rate limit.
/// * `priority_boost` - Optional priority boosting configuration. If set, commands
///   approaching their deadline will have their priority automatically increased.
///
/// # Example
///
/// ```rust,ignore
/// use a3s_lane::{LaneConfig, RetryPolicy};
/// use std::time::Duration;
///
/// // High-priority lane with timeout and retries
/// let config = LaneConfig::new(2, 8)
///     .with_timeout(Duration::from_secs(30))
///     .with_retry_policy(RetryPolicy::exponential(3));
/// ```
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
    ///
    /// # Arguments
    ///
    /// * `min_concurrency` - Minimum concurrent commands (reserved capacity)
    /// * `max_concurrency` - Maximum concurrent commands (capacity limit)
    ///
    /// # Example
    ///
    /// ```rust
    /// use a3s_lane::LaneConfig;
    ///
    /// // Lane with 1-10 concurrent commands
    /// let config = LaneConfig::new(1, 10);
    /// ```
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
    ///
    /// Commands in this lane will be cancelled if they exceed the specified duration.
    ///
    /// # Example
    ///
    /// ```rust
    /// use a3s_lane::LaneConfig;
    /// use std::time::Duration;
    ///
    /// let config = LaneConfig::new(1, 5)
    ///     .with_timeout(Duration::from_secs(30));
    /// ```
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = Some(timeout);
        self
    }

    /// Set retry policy (builder pattern)
    ///
    /// Failed commands will be retried according to the specified policy.
    ///
    /// # Example
    ///
    /// ```rust
    /// use a3s_lane::{LaneConfig, RetryPolicy};
    ///
    /// // Retry up to 3 times with exponential backoff
    /// let config = LaneConfig::new(1, 5)
    ///     .with_retry_policy(RetryPolicy::exponential(3));
    /// ```
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Set rate limit (builder pattern)
    ///
    /// Commands will be throttled according to the specified rate limit.
    ///
    /// # Example
    ///
    /// ```rust
    /// use a3s_lane::{LaneConfig, RateLimitConfig};
    ///
    /// // Limit to 100 commands per second
    /// let config = LaneConfig::new(1, 10)
    ///     .with_rate_limit(RateLimitConfig::per_second(100));
    /// ```
    pub fn with_rate_limit(mut self, rate_limit: RateLimitConfig) -> Self {
        self.rate_limit = Some(rate_limit);
        self
    }

    /// Set priority boost (builder pattern)
    ///
    /// Commands approaching their deadline will have their priority automatically increased.
    ///
    /// # Example
    ///
    /// ```rust
    /// use a3s_lane::{LaneConfig, PriorityBoostConfig};
    /// use std::time::Duration;
    ///
    /// // Boost priority as deadline approaches (5 minute deadline)
    /// let config = LaneConfig::new(1, 10)
    ///     .with_priority_boost(PriorityBoostConfig::standard(Duration::from_secs(300)));
    /// ```
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
