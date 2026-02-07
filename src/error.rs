//! Error types for the lane queue system
//!
//! This module defines the error types used throughout the lane queue system.
//! All errors implement the `std::error::Error` trait via `thiserror::Error`.
//!
//! # Error Handling
//!
//! The [`LaneError`] enum covers all possible error conditions:
//! - Lane configuration errors (lane not found, invalid config)
//! - Queue operation errors (capacity exceeded, shutdown in progress)
//! - Command execution errors (timeout, execution failure)
//!
//! # Example
//!
//! ```rust,ignore
//! use a3s_lane::{QueueManager, LaneError};
//!
//! match manager.submit("query", cmd).await {
//!     Ok(rx) => { /* handle success */ },
//!     Err(LaneError::LaneNotFound(id)) => {
//!         eprintln!("Lane '{}' does not exist", id);
//!     },
//!     Err(LaneError::ShutdownInProgress) => {
//!         eprintln!("Queue is shutting down");
//!     },
//!     Err(e) => {
//!         eprintln!("Unexpected error: {}", e);
//!     }
//! }
//! ```

use thiserror::Error;

/// Lane queue error type
///
/// Represents all possible errors that can occur in the lane queue system.
///
/// # Variants
///
/// * `LaneNotFound` - The specified lane ID does not exist in the queue
/// * `QueueError` - General queue operation error (e.g., capacity exceeded)
/// * `ConfigError` - Invalid configuration (e.g., min > max concurrency)
/// * `CommandError` - Command execution failed
/// * `Timeout` - Command exceeded its timeout duration
/// * `ShutdownInProgress` - Queue is shutting down and not accepting new commands
/// * `Other` - Catch-all for unexpected errors
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

    /// Command timeout
    #[error("Command timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Shutdown in progress
    #[error("Queue is shutting down, not accepting new commands")]
    ShutdownInProgress,

    /// Other error
    #[error("{0}")]
    Other(String),
}

/// Result type alias using LaneError
///
/// Convenience type alias for `std::result::Result<T, LaneError>`.
/// Used throughout the library for consistent error handling.
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
        assert_eq!(
            error.to_string(),
            "Configuration error: invalid concurrency"
        );
    }

    #[test]
    fn test_command_error() {
        let error = LaneError::CommandError("execution failed".to_string());
        assert_eq!(
            error.to_string(),
            "Command execution error: execution failed"
        );
    }

    #[test]
    fn test_timeout_error() {
        let error = LaneError::Timeout(std::time::Duration::from_secs(5));
        assert_eq!(error.to_string(), "Command timed out after 5s");
    }

    #[test]
    fn test_shutdown_in_progress_error() {
        let error = LaneError::ShutdownInProgress;
        assert_eq!(
            error.to_string(),
            "Queue is shutting down, not accepting new commands"
        );
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
