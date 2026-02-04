//! Retry policy for failed commands

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Retry policy configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts
    pub max_retries: u32,
    /// Initial delay before first retry
    pub initial_delay: Duration,
    /// Maximum delay between retries
    pub max_delay: Duration,
    /// Multiplier for exponential backoff
    pub multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

impl RetryPolicy {
    /// Create a retry policy with exponential backoff
    pub fn exponential(max_retries: u32) -> Self {
        Self {
            max_retries,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }

    /// Create a retry policy with fixed delay
    pub fn fixed(max_retries: u32, delay: Duration) -> Self {
        Self {
            max_retries,
            initial_delay: delay,
            max_delay: delay,
            multiplier: 1.0,
        }
    }

    /// Create a no-retry policy
    pub fn none() -> Self {
        Self {
            max_retries: 0,
            initial_delay: Duration::from_secs(0),
            max_delay: Duration::from_secs(0),
            multiplier: 1.0,
        }
    }

    /// Calculate delay for a given retry attempt
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 || self.max_retries == 0 {
            return Duration::from_secs(0);
        }

        let delay_ms = (self.initial_delay.as_millis() as f64)
            * self.multiplier.powi((attempt - 1) as i32);
        let delay = Duration::from_millis(delay_ms as u64);

        delay.min(self.max_delay)
    }

    /// Check if retry is allowed for the given attempt
    pub fn should_retry(&self, attempt: u32) -> bool {
        attempt < self.max_retries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_policy_none() {
        let policy = RetryPolicy::none();
        assert_eq!(policy.max_retries, 0);
        assert!(!policy.should_retry(0));
        assert!(!policy.should_retry(1));
    }

    #[test]
    fn test_retry_policy_exponential() {
        let policy = RetryPolicy::exponential(3);
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.initial_delay, Duration::from_millis(100));
        assert_eq!(policy.max_delay, Duration::from_secs(30));
        assert_eq!(policy.multiplier, 2.0);
    }

    #[test]
    fn test_retry_policy_fixed() {
        let policy = RetryPolicy::fixed(5, Duration::from_secs(1));
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.initial_delay, Duration::from_secs(1));
        assert_eq!(policy.max_delay, Duration::from_secs(1));
        assert_eq!(policy.multiplier, 1.0);
    }

    #[test]
    fn test_should_retry() {
        let policy = RetryPolicy::exponential(3);
        assert!(policy.should_retry(0));
        assert!(policy.should_retry(1));
        assert!(policy.should_retry(2));
        assert!(!policy.should_retry(3));
        assert!(!policy.should_retry(4));
    }

    #[test]
    fn test_delay_for_attempt_exponential() {
        let policy = RetryPolicy::exponential(5);

        // Attempt 0 should have no delay
        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(0));

        // Attempt 1: 100ms * 2^0 = 100ms
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(100));

        // Attempt 2: 100ms * 2^1 = 200ms
        assert_eq!(policy.delay_for_attempt(2), Duration::from_millis(200));

        // Attempt 3: 100ms * 2^2 = 400ms
        assert_eq!(policy.delay_for_attempt(3), Duration::from_millis(400));

        // Attempt 4: 100ms * 2^3 = 800ms
        assert_eq!(policy.delay_for_attempt(4), Duration::from_millis(800));
    }

    #[test]
    fn test_delay_for_attempt_fixed() {
        let policy = RetryPolicy::fixed(3, Duration::from_secs(2));

        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(0));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(2));
    }

    #[test]
    fn test_delay_respects_max() {
        let policy = RetryPolicy {
            max_retries: 10,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            multiplier: 2.0,
        };

        // Attempt 1: 1s
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(1));

        // Attempt 2: 2s
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(2));

        // Attempt 3: 4s
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(4));

        // Attempt 4: would be 8s, but capped at 5s
        assert_eq!(policy.delay_for_attempt(4), Duration::from_secs(5));

        // Attempt 5: would be 16s, but capped at 5s
        assert_eq!(policy.delay_for_attempt(5), Duration::from_secs(5));
    }

    #[test]
    fn test_retry_policy_default() {
        let policy = RetryPolicy::default();
        assert_eq!(policy, RetryPolicy::none());
    }

    #[test]
    fn test_retry_policy_clone() {
        let policy = RetryPolicy::exponential(3);
        let cloned = policy.clone();
        assert_eq!(cloned.max_retries, 3);
        assert_eq!(cloned.initial_delay, Duration::from_millis(100));
    }

    #[test]
    fn test_retry_policy_serialization() {
        let policy = RetryPolicy::exponential(3);
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: RetryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.max_retries, 3);
    }
}
