//! Lane configuration types

use serde::{Deserialize, Serialize};

/// Lane configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LaneConfig {
    /// Minimum concurrency (reserved slots)
    pub min_concurrency: usize,
    /// Maximum concurrency (capacity limit)
    pub max_concurrency: usize,
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            min_concurrency: 1,
            max_concurrency: 4,
        }
    }
}

impl LaneConfig {
    /// Create a new lane configuration
    pub fn new(min_concurrency: usize, max_concurrency: usize) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
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
    }

    #[test]
    fn test_lane_config_new() {
        let config = LaneConfig::new(2, 8);
        assert_eq!(config.min_concurrency, 2);
        assert_eq!(config.max_concurrency, 8);
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
