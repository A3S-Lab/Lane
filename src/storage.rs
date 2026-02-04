//! Persistent storage for queue state
//!
//! This module provides a pluggable storage interface for persisting queue state,
//! including pending commands and dead letter queue entries.

use crate::error::{LaneError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::fs;
use tokio::sync::RwLock;

/// Unique identifier for a stored command
pub type StoredCommandId = String;

/// Serializable command metadata for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCommand {
    /// Unique command ID
    pub id: StoredCommandId,
    /// Command type identifier
    pub command_type: String,
    /// Lane ID where command is queued
    pub lane_id: String,
    /// Serialized command payload (JSON)
    pub payload: serde_json::Value,
    /// Number of retry attempts
    pub retry_count: u32,
    /// Timestamp when command was created
    pub created_at: DateTime<Utc>,
    /// Timestamp when command was last attempted
    pub last_attempt_at: Option<DateTime<Utc>>,
}

/// Serializable dead letter entry for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDeadLetter {
    /// Command ID
    pub command_id: String,
    /// Command type
    pub command_type: String,
    /// Lane ID
    pub lane_id: String,
    /// Error message
    pub error: String,
    /// Number of attempts before failure
    pub attempts: u32,
    /// Timestamp when command failed
    pub failed_at: DateTime<Utc>,
}

/// Storage interface for persisting queue state
#[async_trait]
pub trait Storage: Send + Sync {
    /// Save a command to storage
    async fn save_command(&self, command: StoredCommand) -> Result<()>;

    /// Load all pending commands from storage
    async fn load_commands(&self) -> Result<Vec<StoredCommand>>;

    /// Remove a command from storage (when completed or failed)
    async fn remove_command(&self, id: &str) -> Result<()>;

    /// Save a dead letter entry
    async fn save_dead_letter(&self, letter: StoredDeadLetter) -> Result<()>;

    /// Load all dead letter entries
    async fn load_dead_letters(&self) -> Result<Vec<StoredDeadLetter>>;

    /// Clear all dead letter entries
    async fn clear_dead_letters(&self) -> Result<()>;

    /// Clear all storage (commands and dead letters)
    async fn clear_all(&self) -> Result<()>;
}

/// Local filesystem-based storage implementation
pub struct LocalStorage {
    /// Directory path for storage files
    storage_dir: PathBuf,
    /// In-memory cache of commands (for fast access)
    commands: RwLock<HashMap<String, StoredCommand>>,
    /// In-memory cache of dead letters
    dead_letters: RwLock<Vec<StoredDeadLetter>>,
}

impl LocalStorage {
    /// Create a new LocalStorage with the specified directory
    ///
    /// The directory will be created if it doesn't exist.
    pub async fn new(storage_dir: PathBuf) -> Result<Self> {
        // Create storage directory if it doesn't exist
        fs::create_dir_all(&storage_dir)
            .await
            .map_err(|e| LaneError::Other(format!("Failed to create storage directory: {}", e)))?;

        let storage = Self {
            storage_dir,
            commands: RwLock::new(HashMap::new()),
            dead_letters: RwLock::new(Vec::new()),
        };

        // Load existing data from disk
        storage.load_from_disk().await?;

        Ok(storage)
    }

    /// Get path to commands file
    fn commands_path(&self) -> PathBuf {
        self.storage_dir.join("commands.json")
    }

    /// Get path to dead letters file
    fn dead_letters_path(&self) -> PathBuf {
        self.storage_dir.join("dead_letters.json")
    }

    /// Load data from disk into memory
    async fn load_from_disk(&self) -> Result<()> {
        // Load commands
        if let Ok(data) = fs::read_to_string(self.commands_path()).await {
            if let Ok(commands) = serde_json::from_str::<Vec<StoredCommand>>(&data) {
                let mut cache = self.commands.write().await;
                for cmd in commands {
                    cache.insert(cmd.id.clone(), cmd);
                }
            }
        }

        // Load dead letters
        if let Ok(data) = fs::read_to_string(self.dead_letters_path()).await {
            if let Ok(letters) = serde_json::from_str::<Vec<StoredDeadLetter>>(&data) {
                let mut cache = self.dead_letters.write().await;
                *cache = letters;
            }
        }

        Ok(())
    }

    /// Persist commands to disk
    async fn persist_commands(&self) -> Result<()> {
        let commands = self.commands.read().await;
        let commands_vec: Vec<_> = commands.values().cloned().collect();
        let json = serde_json::to_string_pretty(&commands_vec)
            .map_err(|e| LaneError::Other(format!("Failed to serialize commands: {}", e)))?;

        fs::write(self.commands_path(), json)
            .await
            .map_err(|e| LaneError::Other(format!("Failed to write commands file: {}", e)))?;

        Ok(())
    }

    /// Persist dead letters to disk
    async fn persist_dead_letters(&self) -> Result<()> {
        let letters = self.dead_letters.read().await;
        let json = serde_json::to_string_pretty(&*letters)
            .map_err(|e| LaneError::Other(format!("Failed to serialize dead letters: {}", e)))?;

        fs::write(self.dead_letters_path(), json)
            .await
            .map_err(|e| LaneError::Other(format!("Failed to write dead letters file: {}", e)))?;

        Ok(())
    }
}

#[async_trait]
impl Storage for LocalStorage {
    async fn save_command(&self, command: StoredCommand) -> Result<()> {
        let mut commands = self.commands.write().await;
        commands.insert(command.id.clone(), command);
        drop(commands);

        self.persist_commands().await
    }

    async fn load_commands(&self) -> Result<Vec<StoredCommand>> {
        let commands = self.commands.read().await;
        Ok(commands.values().cloned().collect())
    }

    async fn remove_command(&self, id: &str) -> Result<()> {
        let mut commands = self.commands.write().await;
        commands.remove(id);
        drop(commands);

        self.persist_commands().await
    }

    async fn save_dead_letter(&self, letter: StoredDeadLetter) -> Result<()> {
        let mut letters = self.dead_letters.write().await;
        letters.push(letter);
        drop(letters);

        self.persist_dead_letters().await
    }

    async fn load_dead_letters(&self) -> Result<Vec<StoredDeadLetter>> {
        let letters = self.dead_letters.read().await;
        Ok(letters.clone())
    }

    async fn clear_dead_letters(&self) -> Result<()> {
        let mut letters = self.dead_letters.write().await;
        letters.clear();
        drop(letters);

        self.persist_dead_letters().await
    }

    async fn clear_all(&self) -> Result<()> {
        // Clear in-memory caches
        {
            let mut commands = self.commands.write().await;
            commands.clear();
        }
        {
            let mut letters = self.dead_letters.write().await;
            letters.clear();
        }

        // Delete files
        let _ = fs::remove_file(self.commands_path()).await;
        let _ = fs::remove_file(self.dead_letters_path()).await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_local_storage_save_and_load_commands() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        // Save a command
        let cmd = StoredCommand {
            id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            payload: serde_json::json!({"data": "test"}),
            retry_count: 0,
            created_at: Utc::now(),
            last_attempt_at: None,
        };

        storage.save_command(cmd.clone()).await.unwrap();

        // Load commands
        let loaded = storage.load_commands().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "cmd1");
        assert_eq!(loaded[0].command_type, "test");

        // Create new storage instance (simulates restart)
        let storage2 = LocalStorage::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();
        let loaded2 = storage2.load_commands().await.unwrap();
        assert_eq!(loaded2.len(), 1);
        assert_eq!(loaded2[0].id, "cmd1");
    }

    #[tokio::test]
    async fn test_local_storage_remove_command() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        // Save commands
        let cmd1 = StoredCommand {
            id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            payload: serde_json::json!({}),
            retry_count: 0,
            created_at: Utc::now(),
            last_attempt_at: None,
        };
        let cmd2 = StoredCommand {
            id: "cmd2".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            payload: serde_json::json!({}),
            retry_count: 0,
            created_at: Utc::now(),
            last_attempt_at: None,
        };

        storage.save_command(cmd1).await.unwrap();
        storage.save_command(cmd2).await.unwrap();

        // Remove one command
        storage.remove_command("cmd1").await.unwrap();

        // Verify only cmd2 remains
        let loaded = storage.load_commands().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "cmd2");
    }

    #[tokio::test]
    async fn test_local_storage_dead_letters() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        // Save dead letter
        let letter = StoredDeadLetter {
            command_id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            error: "timeout".to_string(),
            attempts: 3,
            failed_at: Utc::now(),
        };

        storage.save_dead_letter(letter.clone()).await.unwrap();

        // Load dead letters
        let loaded = storage.load_dead_letters().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].command_id, "cmd1");
        assert_eq!(loaded[0].error, "timeout");

        // Clear dead letters
        storage.clear_dead_letters().await.unwrap();
        let loaded = storage.load_dead_letters().await.unwrap();
        assert_eq!(loaded.len(), 0);
    }

    #[tokio::test]
    async fn test_local_storage_clear_all() {
        let temp_dir = TempDir::new().unwrap();
        let storage = LocalStorage::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        // Add command and dead letter
        let cmd = StoredCommand {
            id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            payload: serde_json::json!({}),
            retry_count: 0,
            created_at: Utc::now(),
            last_attempt_at: None,
        };
        let letter = StoredDeadLetter {
            command_id: "cmd1".to_string(),
            command_type: "test".to_string(),
            lane_id: "query".to_string(),
            error: "failed".to_string(),
            attempts: 1,
            failed_at: Utc::now(),
        };

        storage.save_command(cmd).await.unwrap();
        storage.save_dead_letter(letter).await.unwrap();

        // Clear all
        storage.clear_all().await.unwrap();

        // Verify everything is cleared
        let commands = storage.load_commands().await.unwrap();
        let letters = storage.load_dead_letters().await.unwrap();
        assert_eq!(commands.len(), 0);
        assert_eq!(letters.len(), 0);
    }
}
