//! Error types for the lane queue system

use thiserror::Error;

/// Lane queue error type
#[derive(Error, Debug)]
pub enum LaneError {
    /// Lane not found
    #[error("Lane not found: {0}")]
    LaneNotFound(String),

    /// Queue error
    #[error("Queue error: {0}")]
    QueueError(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// Command execution error
    #[error("Command execution error: {0}")]
    CommandError(String),

    /// Other error
    #[error("{0}")]
    Other(String),
}

/// Result type alias using LaneError
pub type Result<T> = std::result::Result<T, LaneError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lane_not_found_error() {
        let error = LaneError::LaneNotFound("query".to_string());
        assert_eq!(error.to_string(), "Lane not found: query");
    }

    #[test]
    fn test_queue_error() {
        let error = LaneError::QueueError("capacity exceeded".to_string());
        assert_eq!(error.to_string(), "Queue error: capacity exceeded");
    }

    #[test]
    fn test_config_error() {
        let error = LaneError::ConfigError("invalid concurrency".to_string());
        assert_eq!(error.to_string(), "Configuration error: invalid concurrency");
    }

    #[test]
    fn test_command_error() {
        let error = LaneError::CommandError("execution failed".to_string());
        assert_eq!(error.to_string(), "Command execution error: execution failed");
    }

    #[test]
    fn test_other_error() {
        let error = LaneError::Other("unexpected error".to_string());
        assert_eq!(error.to_string(), "unexpected error");
    }

    #[test]
    fn test_error_debug() {
        let error = LaneError::LaneNotFound("test".to_string());
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("LaneNotFound"));
    }
}
