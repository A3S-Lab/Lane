//! Dead Letter Queue for permanently failed commands

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::queue::{CommandId, LaneId};

/// A dead letter represents a command that has permanently failed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetter {
    /// Command ID
    pub command_id: CommandId,
    /// Command type
    pub command_type: String,
    /// Lane ID where the command was executed
    pub lane_id: LaneId,
    /// Error message
    pub error: String,
    /// Number of attempts made
    pub attempts: u32,
    /// Timestamp when the command failed
    pub failed_at: DateTime<Utc>,
}

/// Dead Letter Queue for storing permanently failed commands
#[derive(Clone)]
pub struct DeadLetterQueue {
    letters: Arc<Mutex<VecDeque<DeadLetter>>>,
    max_size: usize,
}

impl DeadLetterQueue {
    /// Create a new Dead Letter Queue with a maximum size
    pub fn new(max_size: usize) -> Self {
        Self {
            letters: Arc::new(Mutex::new(VecDeque::new())),
            max_size,
        }
    }

    /// Push a dead letter to the queue
    pub async fn push(&self, letter: DeadLetter) {
        let mut letters = self.letters.lock().await;

        // If at capacity, remove oldest
        if letters.len() >= self.max_size {
            letters.pop_front();
        }

        letters.push_back(letter);
    }

    /// Pop the oldest dead letter from the queue
    pub async fn pop(&self) -> Option<DeadLetter> {
        let mut letters = self.letters.lock().await;
        letters.pop_front()
    }

    /// List all dead letters (returns a copy)
    pub async fn list(&self) -> Vec<DeadLetter> {
        let letters = self.letters.lock().await;
        letters.iter().cloned().collect()
    }

    /// Clear all dead letters
    pub async fn clear(&self) {
        let mut letters = self.letters.lock().await;
        letters.clear();
    }

    /// Get the number of dead letters
    pub async fn len(&self) -> usize {
        let letters = self.letters.lock().await;
        letters.len()
    }

    /// Check if the queue is empty
    pub async fn is_empty(&self) -> bool {
        let letters = self.letters.lock().await;
        letters.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dead_letter(id: &str, error: &str) -> DeadLetter {
        DeadLetter {
            command_id: id.to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            error: error.to_string(),
            attempts: 3,
            failed_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_dlq_new() {
        let dlq = DeadLetterQueue::new(100);
        assert_eq!(dlq.max_size, 100);
        assert!(dlq.is_empty().await);
    }

    #[tokio::test]
    async fn test_dlq_push_and_len() {
        let dlq = DeadLetterQueue::new(10);

        dlq.push(make_dead_letter("cmd1", "error1")).await;
        assert_eq!(dlq.len().await, 1);

        dlq.push(make_dead_letter("cmd2", "error2")).await;
        assert_eq!(dlq.len().await, 2);
    }

    #[tokio::test]
    async fn test_dlq_pop() {
        let dlq = DeadLetterQueue::new(10);

        dlq.push(make_dead_letter("cmd1", "error1")).await;
        dlq.push(make_dead_letter("cmd2", "error2")).await;

        let letter = dlq.pop().await.unwrap();
        assert_eq!(letter.command_id, "cmd1");
        assert_eq!(dlq.len().await, 1);

        let letter = dlq.pop().await.unwrap();
        assert_eq!(letter.command_id, "cmd2");
        assert!(dlq.is_empty().await);

        assert!(dlq.pop().await.is_none());
    }

    #[tokio::test]
    async fn test_dlq_list() {
        let dlq = DeadLetterQueue::new(10);

        dlq.push(make_dead_letter("cmd1", "error1")).await;
        dlq.push(make_dead_letter("cmd2", "error2")).await;
        dlq.push(make_dead_letter("cmd3", "error3")).await;

        let list = dlq.list().await;
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].command_id, "cmd1");
        assert_eq!(list[1].command_id, "cmd2");
        assert_eq!(list[2].command_id, "cmd3");

        // List should not remove items
        assert_eq!(dlq.len().await, 3);
    }

    #[tokio::test]
    async fn test_dlq_clear() {
        let dlq = DeadLetterQueue::new(10);

        dlq.push(make_dead_letter("cmd1", "error1")).await;
        dlq.push(make_dead_letter("cmd2", "error2")).await;

        assert_eq!(dlq.len().await, 2);

        dlq.clear().await;
        assert!(dlq.is_empty().await);
    }

    #[tokio::test]
    async fn test_dlq_max_size() {
        let dlq = DeadLetterQueue::new(3);

        dlq.push(make_dead_letter("cmd1", "error1")).await;
        dlq.push(make_dead_letter("cmd2", "error2")).await;
        dlq.push(make_dead_letter("cmd3", "error3")).await;

        assert_eq!(dlq.len().await, 3);

        // Push 4th item - should evict oldest
        dlq.push(make_dead_letter("cmd4", "error4")).await;
        assert_eq!(dlq.len().await, 3);

        let list = dlq.list().await;
        assert_eq!(list[0].command_id, "cmd2"); // cmd1 was evicted
        assert_eq!(list[1].command_id, "cmd3");
        assert_eq!(list[2].command_id, "cmd4");
    }

    #[tokio::test]
    async fn test_dlq_is_empty() {
        let dlq = DeadLetterQueue::new(10);

        assert!(dlq.is_empty().await);

        dlq.push(make_dead_letter("cmd1", "error1")).await;
        assert!(!dlq.is_empty().await);

        dlq.clear().await;
        assert!(dlq.is_empty().await);
    }

    #[tokio::test]
    async fn test_dead_letter_serialization() {
        let letter = make_dead_letter("cmd1", "test error");

        let json = serde_json::to_string(&letter).unwrap();
        let parsed: DeadLetter = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.command_id, "cmd1");
        assert_eq!(parsed.error, "test error");
        assert_eq!(parsed.attempts, 3);
    }

    #[tokio::test]
    async fn test_dead_letter_clone() {
        let letter = make_dead_letter("cmd1", "error1");
        let cloned = letter.clone();

        assert_eq!(cloned.command_id, letter.command_id);
        assert_eq!(cloned.error, letter.error);
    }

    #[tokio::test]
    async fn test_dlq_clone() {
        let dlq1 = DeadLetterQueue::new(10);
        dlq1.push(make_dead_letter("cmd1", "error1")).await;

        let dlq2 = dlq1.clone();
        assert_eq!(dlq2.len().await, 1);

        // Both should share the same underlying data
        dlq2.push(make_dead_letter("cmd2", "error2")).await;
        assert_eq!(dlq1.len().await, 2);
        assert_eq!(dlq2.len().await, 2);
    }
}
