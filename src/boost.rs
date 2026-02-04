//! Priority boosting for deadline-based priority adjustment
//!
//! This module provides priority boosting capabilities that automatically
//! increase command priority as deadlines approach.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::time::Duration;

/// Priority boost configuration
#[derive(Debug, Clone, PartialEq)]
pub struct PriorityBoostConfig {
    /// Enable priority boosting
    pub enabled: bool,
    /// Deadline duration from command creation
    pub deadline: Duration,
    /// Boost intervals - (time_before_deadline, priority_boost)
    /// Priority boost is subtracted from current priority (lower = higher priority)
    pub boost_intervals: Vec<(Duration, u8)>,
}

impl PriorityBoostConfig {
    /// Create a new priority boost configuration
    pub fn new(deadline: Duration) -> Self {
        Self {
            enabled: true,
            deadline,
            boost_intervals: Vec::new(),
        }
    }

    /// Create a disabled priority boost configuration
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            deadline: Duration::ZERO,
            boost_intervals: Vec::new(),
        }
    }

    /// Add a boost interval
    ///
    /// When time remaining before deadline is less than `time_before_deadline`,
    /// the priority will be boosted by `boost` (subtracted from current priority).
    pub fn with_boost(mut self, time_before_deadline: Duration, boost: u8) -> Self {
        self.boost_intervals.push((time_before_deadline, boost));
        // Sort by time (descending) so we check largest intervals first
        self.boost_intervals
            .sort_by(|a, b| b.0.cmp(&a.0));
        self
    }

    /// Create a standard boost configuration with common intervals
    ///
    /// - At 75% of deadline: boost by 1
    /// - At 50% of deadline: boost by 2
    /// - At 25% of deadline: boost by 3
    /// - At 10% of deadline: boost by 4
    pub fn standard(deadline: Duration) -> Self {
        Self::new(deadline)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.75), 1)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.50), 2)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.25), 3)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.10), 4)
    }

    /// Create an aggressive boost configuration
    ///
    /// - At 80% of deadline: boost by 2
    /// - At 60% of deadline: boost by 4
    /// - At 40% of deadline: boost by 6
    /// - At 20% of deadline: boost by 8
    pub fn aggressive(deadline: Duration) -> Self {
        Self::new(deadline)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.80), 2)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.60), 4)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.40), 6)
            .with_boost(Duration::from_secs_f64(deadline.as_secs_f64() * 0.20), 8)
    }
}

impl Default for PriorityBoostConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

/// Priority booster calculates dynamic priority based on deadline
pub struct PriorityBooster {
    config: PriorityBoostConfig,
}

impl PriorityBooster {
    /// Create a new priority booster
    pub fn new(config: PriorityBoostConfig) -> Self {
        Self { config }
    }

    /// Calculate the boosted priority for a command
    ///
    /// Returns the adjusted priority (lower = higher priority).
    /// The boost is subtracted from the base priority, with a minimum of 0.
    pub fn calculate_priority(&self, base_priority: u8, created_at: DateTime<Utc>) -> u8 {
        if !self.config.enabled {
            return base_priority;
        }

        let now = Utc::now();
        let deadline = created_at
            + ChronoDuration::from_std(self.config.deadline).unwrap_or(ChronoDuration::zero());
        let time_remaining = deadline.signed_duration_since(now);

        // If past deadline, give maximum boost
        if time_remaining <= ChronoDuration::zero() {
            return 0; // Highest priority
        }

        let time_remaining_std =
            Duration::from_millis(time_remaining.num_milliseconds().max(0) as u64);

        // Find the applicable boost
        let mut boost: u8 = 0;
        for (threshold, boost_amount) in &self.config.boost_intervals {
            if time_remaining_std <= *threshold {
                boost = *boost_amount;
                // Don't break - continue to find the highest applicable boost
            }
        }

        base_priority.saturating_sub(boost)
    }

    /// Check if a command has exceeded its deadline
    pub fn is_past_deadline(&self, created_at: DateTime<Utc>) -> bool {
        if !self.config.enabled {
            return false;
        }

        let now = Utc::now();
        let deadline = created_at
            + ChronoDuration::from_std(self.config.deadline).unwrap_or(ChronoDuration::zero());

        now >= deadline
    }

    /// Get time remaining until deadline
    pub fn time_remaining(&self, created_at: DateTime<Utc>) -> Option<Duration> {
        if !self.config.enabled {
            return None;
        }

        let now = Utc::now();
        let deadline = created_at
            + ChronoDuration::from_std(self.config.deadline).unwrap_or(ChronoDuration::zero());
        let remaining = deadline.signed_duration_since(now);

        if remaining <= ChronoDuration::zero() {
            Some(Duration::ZERO)
        } else {
            Some(Duration::from_millis(remaining.num_milliseconds() as u64))
        }
    }

    /// Get the configuration
    pub fn config(&self) -> &PriorityBoostConfig {
        &self.config
    }
}

impl Default for PriorityBooster {
    fn default() -> Self {
        Self::new(PriorityBoostConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_boost_config_new() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60));
        assert!(config.enabled);
        assert_eq!(config.deadline, Duration::from_secs(60));
        assert!(config.boost_intervals.is_empty());
    }

    #[test]
    fn test_priority_boost_config_disabled() {
        let config = PriorityBoostConfig::disabled();
        assert!(!config.enabled);
    }

    #[test]
    fn test_priority_boost_config_with_boost() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60))
            .with_boost(Duration::from_secs(30), 1)
            .with_boost(Duration::from_secs(15), 2);

        assert_eq!(config.boost_intervals.len(), 2);
        // Should be sorted descending by time
        assert_eq!(config.boost_intervals[0].0, Duration::from_secs(30));
        assert_eq!(config.boost_intervals[1].0, Duration::from_secs(15));
    }

    #[test]
    fn test_priority_boost_config_standard() {
        let config = PriorityBoostConfig::standard(Duration::from_secs(100));
        assert!(config.enabled);
        assert_eq!(config.deadline, Duration::from_secs(100));
        assert_eq!(config.boost_intervals.len(), 4);
    }

    #[test]
    fn test_priority_boost_config_aggressive() {
        let config = PriorityBoostConfig::aggressive(Duration::from_secs(100));
        assert!(config.enabled);
        assert_eq!(config.boost_intervals.len(), 4);
        // Aggressive should have higher boost values
        assert!(config.boost_intervals.iter().any(|(_, b)| *b >= 6));
    }

    #[test]
    fn test_priority_booster_disabled() {
        let booster = PriorityBooster::new(PriorityBoostConfig::disabled());
        let created_at = Utc::now() - ChronoDuration::hours(1);

        // Should return base priority unchanged
        assert_eq!(booster.calculate_priority(5, created_at), 5);
        assert!(!booster.is_past_deadline(created_at));
        assert!(booster.time_remaining(created_at).is_none());
    }

    #[test]
    fn test_priority_booster_no_boost_before_threshold() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60))
            .with_boost(Duration::from_secs(30), 2);
        let booster = PriorityBooster::new(config);

        // Created just now, 60 seconds until deadline
        let created_at = Utc::now();

        // Time remaining (60s) > threshold (30s), so no boost
        assert_eq!(booster.calculate_priority(5, created_at), 5);
    }

    #[test]
    fn test_priority_booster_boost_after_threshold() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60))
            .with_boost(Duration::from_secs(30), 2);
        let booster = PriorityBooster::new(config);

        // Created 40 seconds ago, 20 seconds until deadline
        let created_at = Utc::now() - ChronoDuration::seconds(40);

        // Time remaining (20s) < threshold (30s), so boost by 2
        assert_eq!(booster.calculate_priority(5, created_at), 3);
    }

    #[test]
    fn test_priority_booster_past_deadline() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60))
            .with_boost(Duration::from_secs(30), 2);
        let booster = PriorityBooster::new(config);

        // Created 2 minutes ago, past deadline
        let created_at = Utc::now() - ChronoDuration::seconds(120);

        // Past deadline, should get maximum priority (0)
        assert_eq!(booster.calculate_priority(5, created_at), 0);
        assert!(booster.is_past_deadline(created_at));
    }

    #[test]
    fn test_priority_booster_multiple_thresholds() {
        let config = PriorityBoostConfig::new(Duration::from_secs(100))
            .with_boost(Duration::from_secs(75), 1)
            .with_boost(Duration::from_secs(50), 2)
            .with_boost(Duration::from_secs(25), 3);
        let booster = PriorityBooster::new(config);

        // 80 seconds remaining - no boost
        let created_at = Utc::now() - ChronoDuration::seconds(20);
        assert_eq!(booster.calculate_priority(10, created_at), 10);

        // 60 seconds remaining - boost by 1
        let created_at = Utc::now() - ChronoDuration::seconds(40);
        assert_eq!(booster.calculate_priority(10, created_at), 9);

        // 40 seconds remaining - boost by 2
        let created_at = Utc::now() - ChronoDuration::seconds(60);
        assert_eq!(booster.calculate_priority(10, created_at), 8);

        // 20 seconds remaining - boost by 3
        let created_at = Utc::now() - ChronoDuration::seconds(80);
        assert_eq!(booster.calculate_priority(10, created_at), 7);
    }

    #[test]
    fn test_priority_booster_saturating_sub() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60))
            .with_boost(Duration::from_secs(30), 10); // Boost more than base priority
        let booster = PriorityBooster::new(config);

        // Created 40 seconds ago
        let created_at = Utc::now() - ChronoDuration::seconds(40);

        // Base priority 5, boost 10, should saturate at 0
        assert_eq!(booster.calculate_priority(5, created_at), 0);
    }

    #[test]
    fn test_priority_booster_time_remaining() {
        let config = PriorityBoostConfig::new(Duration::from_secs(60));
        let booster = PriorityBooster::new(config);

        // Created just now
        let created_at = Utc::now();
        let remaining = booster.time_remaining(created_at).unwrap();
        assert!(remaining.as_secs() >= 59 && remaining.as_secs() <= 60);

        // Created 30 seconds ago
        let created_at = Utc::now() - ChronoDuration::seconds(30);
        let remaining = booster.time_remaining(created_at).unwrap();
        assert!(remaining.as_secs() >= 29 && remaining.as_secs() <= 31);

        // Past deadline
        let created_at = Utc::now() - ChronoDuration::seconds(120);
        let remaining = booster.time_remaining(created_at).unwrap();
        assert_eq!(remaining, Duration::ZERO);
    }
}
