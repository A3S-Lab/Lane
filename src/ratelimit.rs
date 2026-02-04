//! Rate limiting for lanes
//!
//! This module provides rate limiting capabilities for controlling
//! command throughput per lane.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Rate limiter configuration
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitConfig {
    /// Maximum number of commands per time window
    pub max_commands: u64,
    /// Time window duration
    pub window: Duration,
}

impl RateLimitConfig {
    /// Create a new rate limit configuration
    pub fn new(max_commands: u64, window: Duration) -> Self {
        Self {
            max_commands,
            window,
        }
    }

    /// Create a rate limit of N commands per second
    pub fn per_second(max_commands: u64) -> Self {
        Self {
            max_commands,
            window: Duration::from_secs(1),
        }
    }

    /// Create a rate limit of N commands per minute
    pub fn per_minute(max_commands: u64) -> Self {
        Self {
            max_commands,
            window: Duration::from_secs(60),
        }
    }

    /// Create a rate limit of N commands per hour
    pub fn per_hour(max_commands: u64) -> Self {
        Self {
            max_commands,
            window: Duration::from_secs(3600),
        }
    }

    /// No rate limiting
    pub fn unlimited() -> Self {
        Self {
            max_commands: u64::MAX,
            window: Duration::from_secs(1),
        }
    }
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Token bucket rate limiter
///
/// Uses the token bucket algorithm for smooth rate limiting.
/// Tokens are added at a constant rate and consumed when commands are processed.
pub struct TokenBucketLimiter {
    /// Maximum tokens (bucket capacity)
    capacity: u64,
    /// Current available tokens
    tokens: AtomicU64,
    /// Tokens added per second
    refill_rate: f64,
    /// Last refill timestamp
    last_refill: Mutex<Instant>,
}

impl TokenBucketLimiter {
    /// Create a new token bucket limiter from rate limit config
    pub fn new(config: &RateLimitConfig) -> Self {
        let refill_rate = config.max_commands as f64 / config.window.as_secs_f64();
        Self {
            capacity: config.max_commands,
            tokens: AtomicU64::new(config.max_commands),
            refill_rate,
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// Try to acquire a token (non-blocking)
    ///
    /// Returns true if a token was acquired, false if rate limited.
    pub async fn try_acquire(&self) -> bool {
        self.refill().await;

        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current == 0 {
                return false;
            }

            if self
                .tokens
                .compare_exchange(current, current - 1, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Acquire a token, waiting if necessary
    ///
    /// Returns the duration waited.
    pub async fn acquire(&self) -> Duration {
        let start = Instant::now();

        loop {
            if self.try_acquire().await {
                return start.elapsed();
            }

            // Calculate wait time for next token
            let wait_time = Duration::from_secs_f64(1.0 / self.refill_rate);
            tokio::time::sleep(wait_time.min(Duration::from_millis(10))).await;
        }
    }

    /// Refill tokens based on elapsed time
    async fn refill(&self) {
        let mut last_refill = self.last_refill.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(*last_refill);

        if elapsed.as_secs_f64() > 0.0 {
            let new_tokens = (elapsed.as_secs_f64() * self.refill_rate) as u64;
            if new_tokens > 0 {
                let current = self.tokens.load(Ordering::Relaxed);
                let new_total = (current + new_tokens).min(self.capacity);
                self.tokens.store(new_total, Ordering::Relaxed);
                *last_refill = now;
            }
        }
    }

    /// Get current available tokens
    pub fn available_tokens(&self) -> u64 {
        self.tokens.load(Ordering::Relaxed)
    }

    /// Get the capacity (max tokens)
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Get the refill rate (tokens per second)
    pub fn refill_rate(&self) -> f64 {
        self.refill_rate
    }
}

/// Sliding window rate limiter
///
/// Uses a sliding window algorithm for more accurate rate limiting.
/// Tracks timestamps of recent commands within the window.
pub struct SlidingWindowLimiter {
    /// Maximum commands per window
    max_commands: u64,
    /// Window duration
    window: Duration,
    /// Timestamps of recent commands
    timestamps: Mutex<Vec<Instant>>,
}

impl SlidingWindowLimiter {
    /// Create a new sliding window limiter from rate limit config
    pub fn new(config: &RateLimitConfig) -> Self {
        Self {
            max_commands: config.max_commands,
            window: config.window,
            timestamps: Mutex::new(Vec::new()),
        }
    }

    /// Try to acquire permission (non-blocking)
    ///
    /// Returns true if allowed, false if rate limited.
    pub async fn try_acquire(&self) -> bool {
        let mut timestamps = self.timestamps.lock().await;
        let now = Instant::now();
        let window_start = now - self.window;

        // Remove expired timestamps
        timestamps.retain(|&ts| ts > window_start);

        // Check if we're under the limit
        if (timestamps.len() as u64) < self.max_commands {
            timestamps.push(now);
            true
        } else {
            false
        }
    }

    /// Acquire permission, waiting if necessary
    ///
    /// Returns the duration waited.
    pub async fn acquire(&self) -> Duration {
        let start = Instant::now();

        loop {
            if self.try_acquire().await {
                return start.elapsed();
            }

            // Wait a bit before retrying
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Get current command count in window
    pub async fn current_count(&self) -> u64 {
        let mut timestamps = self.timestamps.lock().await;
        let now = Instant::now();
        let window_start = now - self.window;

        // Remove expired timestamps
        timestamps.retain(|&ts| ts > window_start);

        timestamps.len() as u64
    }

    /// Get remaining capacity in current window
    pub async fn remaining(&self) -> u64 {
        let count = self.current_count().await;
        self.max_commands.saturating_sub(count)
    }
}

/// Rate limiter enum for different algorithms
#[derive(Clone, Default)]
pub enum RateLimiter {
    /// Token bucket algorithm
    TokenBucket(Arc<TokenBucketLimiter>),
    /// Sliding window algorithm
    SlidingWindow(Arc<SlidingWindowLimiter>),
    /// No rate limiting
    #[default]
    Unlimited,
}

impl RateLimiter {
    /// Create a token bucket rate limiter
    pub fn token_bucket(config: &RateLimitConfig) -> Self {
        Self::TokenBucket(Arc::new(TokenBucketLimiter::new(config)))
    }

    /// Create a sliding window rate limiter
    pub fn sliding_window(config: &RateLimitConfig) -> Self {
        Self::SlidingWindow(Arc::new(SlidingWindowLimiter::new(config)))
    }

    /// Create an unlimited rate limiter
    pub fn unlimited() -> Self {
        Self::Unlimited
    }

    /// Try to acquire permission (non-blocking)
    pub async fn try_acquire(&self) -> bool {
        match self {
            Self::TokenBucket(limiter) => limiter.try_acquire().await,
            Self::SlidingWindow(limiter) => limiter.try_acquire().await,
            Self::Unlimited => true,
        }
    }

    /// Acquire permission, waiting if necessary
    pub async fn acquire(&self) -> Duration {
        match self {
            Self::TokenBucket(limiter) => limiter.acquire().await,
            Self::SlidingWindow(limiter) => limiter.acquire().await,
            Self::Unlimited => Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limit_config_per_second() {
        let config = RateLimitConfig::per_second(100);
        assert_eq!(config.max_commands, 100);
        assert_eq!(config.window, Duration::from_secs(1));
    }

    #[test]
    fn test_rate_limit_config_per_minute() {
        let config = RateLimitConfig::per_minute(1000);
        assert_eq!(config.max_commands, 1000);
        assert_eq!(config.window, Duration::from_secs(60));
    }

    #[test]
    fn test_rate_limit_config_unlimited() {
        let config = RateLimitConfig::unlimited();
        assert_eq!(config.max_commands, u64::MAX);
    }

    #[tokio::test]
    async fn test_token_bucket_limiter_basic() {
        let config = RateLimitConfig::per_second(10);
        let limiter = TokenBucketLimiter::new(&config);

        // Should be able to acquire up to capacity
        for _ in 0..10 {
            assert!(limiter.try_acquire().await);
        }

        // Should be rate limited now
        assert!(!limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn test_token_bucket_limiter_refill() {
        let config = RateLimitConfig::new(10, Duration::from_millis(100));
        let limiter = TokenBucketLimiter::new(&config);

        // Consume all tokens
        for _ in 0..10 {
            assert!(limiter.try_acquire().await);
        }
        assert!(!limiter.try_acquire().await);

        // Wait for refill
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should have some tokens now
        assert!(limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn test_sliding_window_limiter_basic() {
        let config = RateLimitConfig::per_second(5);
        let limiter = SlidingWindowLimiter::new(&config);

        // Should be able to acquire up to limit
        for _ in 0..5 {
            assert!(limiter.try_acquire().await);
        }

        // Should be rate limited now
        assert!(!limiter.try_acquire().await);

        // Check counts
        assert_eq!(limiter.current_count().await, 5);
        assert_eq!(limiter.remaining().await, 0);
    }

    #[tokio::test]
    async fn test_sliding_window_limiter_expiry() {
        let config = RateLimitConfig::new(5, Duration::from_millis(100));
        let limiter = SlidingWindowLimiter::new(&config);

        // Consume all capacity
        for _ in 0..5 {
            assert!(limiter.try_acquire().await);
        }
        assert!(!limiter.try_acquire().await);

        // Wait for window to expire
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Should be able to acquire again
        assert!(limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn test_rate_limiter_enum_token_bucket() {
        let config = RateLimitConfig::per_second(5);
        let limiter = RateLimiter::token_bucket(&config);

        for _ in 0..5 {
            assert!(limiter.try_acquire().await);
        }
        assert!(!limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn test_rate_limiter_enum_sliding_window() {
        let config = RateLimitConfig::per_second(5);
        let limiter = RateLimiter::sliding_window(&config);

        for _ in 0..5 {
            assert!(limiter.try_acquire().await);
        }
        assert!(!limiter.try_acquire().await);
    }

    #[tokio::test]
    async fn test_rate_limiter_enum_unlimited() {
        let limiter = RateLimiter::unlimited();

        // Should always succeed
        for _ in 0..1000 {
            assert!(limiter.try_acquire().await);
        }
    }

    #[tokio::test]
    async fn test_rate_limiter_acquire_waits() {
        let config = RateLimitConfig::new(1, Duration::from_millis(50));
        let limiter = RateLimiter::token_bucket(&config);

        // First acquire should be instant
        let wait1 = limiter.acquire().await;
        assert!(wait1 < Duration::from_millis(10));

        // Second acquire should wait
        let wait2 = limiter.acquire().await;
        assert!(wait2 >= Duration::from_millis(10));
    }
}
