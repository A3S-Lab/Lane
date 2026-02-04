//! Core queue implementation with lanes and priority scheduling

use crate::config::LaneConfig;
use crate::error::{LaneError, Result};
use crate::event::EventEmitter;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

/// Lane identifier
pub type LaneId = String;

/// Command identifier
pub type CommandId = String;

/// Lane priority (lower number = higher priority)
pub type Priority = u8;

/// Lane priorities
pub mod priorities {
    use super::Priority;

    pub const SYSTEM: Priority = 0;
    pub const CONTROL: Priority = 1;
    pub const QUERY: Priority = 2;
    pub const SESSION: Priority = 3;
    pub const SKILL: Priority = 4;
    pub const PROMPT: Priority = 5;
}

/// Command to be executed
#[async_trait]
pub trait Command: Send + Sync {
    /// Execute the command
    async fn execute(&self) -> Result<serde_json::Value>;

    /// Get command type (for logging/debugging)
    fn command_type(&self) -> &str;
}

/// Command wrapper
#[allow(dead_code)]
struct CommandWrapper {
    id: CommandId,
    command: Box<dyn Command>,
    result_tx: tokio::sync::oneshot::Sender<Result<serde_json::Value>>,
}

/// Lane state
#[allow(dead_code)]
struct LaneState {
    /// Lane configuration
    config: LaneConfig,

    /// Priority
    priority: Priority,

    /// Pending commands (FIFO queue)
    pending: VecDeque<CommandWrapper>,

    /// Active command count
    active: usize,

    /// Semaphore for concurrency control
    semaphore: Arc<Semaphore>,
}

impl LaneState {
    fn new(config: LaneConfig, priority: Priority) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrency));
        Self {
            config,
            priority,
            pending: VecDeque::new(),
            active: 0,
            semaphore,
        }
    }

    fn has_capacity(&self) -> bool {
        self.active < self.config.max_concurrency
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Lane
pub struct Lane {
    id: LaneId,
    state: Arc<Mutex<LaneState>>,
}

impl Lane {
    /// Create a new lane
    pub fn new(id: impl Into<String>, config: LaneConfig, priority: Priority) -> Self {
        Self {
            id: id.into(),
            state: Arc::new(Mutex::new(LaneState::new(config, priority))),
        }
    }

    /// Get lane ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get lane priority
    pub async fn priority(&self) -> Priority {
        self.state.lock().await.priority
    }

    /// Enqueue a command
    pub async fn enqueue(
        &self,
        command: Box<dyn Command>,
    ) -> tokio::sync::oneshot::Receiver<Result<serde_json::Value>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let wrapper = CommandWrapper {
            id: Uuid::new_v4().to_string(),
            command,
            result_tx: tx,
        };

        let mut state = self.state.lock().await;
        state.pending.push_back(wrapper);

        rx
    }

    /// Try to dequeue a command for execution
    async fn try_dequeue(&self) -> Option<CommandWrapper> {
        let mut state = self.state.lock().await;
        if state.has_capacity() && state.has_pending() {
            state.active += 1;
            state.pending.pop_front()
        } else {
            None
        }
    }

    /// Mark a command as completed
    async fn mark_completed(&self) {
        let mut state = self.state.lock().await;
        state.active = state.active.saturating_sub(1);
    }

    /// Get lane status
    pub async fn status(&self) -> LaneStatus {
        let state = self.state.lock().await;
        LaneStatus {
            pending: state.pending.len(),
            active: state.active,
            min: state.config.min_concurrency,
            max: state.config.max_concurrency,
        }
    }
}

/// Lane status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneStatus {
    pub pending: usize,
    pub active: usize,
    pub min: usize,
    pub max: usize,
}

/// Command queue
#[allow(dead_code)]
pub struct CommandQueue {
    lanes: Arc<Mutex<HashMap<LaneId, Arc<Lane>>>>,
    event_emitter: EventEmitter,
}

impl CommandQueue {
    /// Create a new command queue
    pub fn new(event_emitter: EventEmitter) -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
        }
    }

    /// Register a lane
    pub async fn register_lane(&self, lane: Arc<Lane>) {
        let mut lanes = self.lanes.lock().await;
        lanes.insert(lane.id().to_string(), lane);
    }

    /// Submit a command to a lane
    pub async fn submit(
        &self,
        lane_id: &str,
        command: Box<dyn Command>,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<serde_json::Value>>> {
        let lanes = self.lanes.lock().await;
        let lane = lanes
            .get(lane_id)
            .ok_or_else(|| LaneError::LaneNotFound(lane_id.to_string()))?;

        Ok(lane.enqueue(command).await)
    }

    /// Start the scheduler
    pub async fn start_scheduler(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.schedule_next().await;
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });
    }

    /// Schedule the next command
    async fn schedule_next(&self) {
        // Find the highest-priority lane with pending commands
        let lanes = self.lanes.lock().await;

        // Collect lanes with their priorities
        let mut lane_priorities = Vec::new();
        for lane in lanes.values() {
            let priority = lane.priority().await;
            lane_priorities.push((priority, Arc::clone(lane)));
        }

        // Sort by priority (lower number = higher priority)
        lane_priorities.sort_by_key(|(priority, _)| *priority);

        for (_, lane) in lane_priorities {
            if let Some(wrapper) = lane.try_dequeue().await {
                let lane_clone = Arc::clone(&lane);
                tokio::spawn(async move {
                    let result = wrapper.command.execute().await;
                    let _ = wrapper.result_tx.send(result);
                    lane_clone.mark_completed().await;
                });
                break;
            }
        }
    }

    /// Get queue status for all lanes
    pub async fn status(&self) -> HashMap<LaneId, LaneStatus> {
        let lanes = self.lanes.lock().await;
        let mut status = HashMap::new();

        for (id, lane) in lanes.iter() {
            status.insert(id.clone(), lane.status().await);
        }

        status
    }
}

/// Built-in lane IDs
pub mod lane_ids {
    pub const SYSTEM: &str = "system";
    pub const CONTROL: &str = "control";
    pub const QUERY: &str = "query";
    pub const SESSION: &str = "session";
    pub const SKILL: &str = "skill";
    pub const PROMPT: &str = "prompt";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test command implementation
    struct TestCommand {
        result: serde_json::Value,
        delay_ms: Option<u64>,
    }

    impl TestCommand {
        fn new(result: serde_json::Value) -> Self {
            Self {
                result,
                delay_ms: None,
            }
        }

        fn with_delay(result: serde_json::Value, delay_ms: u64) -> Self {
            Self {
                result,
                delay_ms: Some(delay_ms),
            }
        }
    }

    #[async_trait]
    impl Command for TestCommand {
        async fn execute(&self) -> Result<serde_json::Value> {
            if let Some(delay) = self.delay_ms {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay)).await;
            }
            Ok(self.result.clone())
        }

        fn command_type(&self) -> &str {
            "test"
        }
    }

    /// Failing test command
    struct FailingCommand {
        message: String,
    }

    #[async_trait]
    impl Command for FailingCommand {
        async fn execute(&self) -> Result<serde_json::Value> {
            Err(LaneError::Other(self.message.clone()))
        }

        fn command_type(&self) -> &str {
            "failing"
        }
    }

    #[test]
    fn test_priorities() {
        assert_eq!(priorities::SYSTEM, 0);
        assert_eq!(priorities::CONTROL, 1);
        assert_eq!(priorities::QUERY, 2);
        assert_eq!(priorities::SESSION, 3);
        assert_eq!(priorities::SKILL, 4);
        assert_eq!(priorities::PROMPT, 5);

        // Verify priority ordering: system has highest priority (lowest number)
        // Using const block to satisfy clippy assertions_on_constants
        const _: () = {
            assert!(priorities::SYSTEM < priorities::CONTROL);
            assert!(priorities::CONTROL < priorities::QUERY);
            assert!(priorities::QUERY < priorities::SESSION);
            assert!(priorities::SESSION < priorities::SKILL);
            assert!(priorities::SKILL < priorities::PROMPT);
        };
    }

    #[test]
    fn test_lane_ids() {
        assert_eq!(lane_ids::SYSTEM, "system");
        assert_eq!(lane_ids::CONTROL, "control");
        assert_eq!(lane_ids::QUERY, "query");
        assert_eq!(lane_ids::SESSION, "session");
        assert_eq!(lane_ids::SKILL, "skill");
        assert_eq!(lane_ids::PROMPT, "prompt");
    }

    #[test]
    fn test_lane_new() {
        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        };
        let lane = Lane::new("test-lane", config, priorities::QUERY);

        assert_eq!(lane.id(), "test-lane");
    }

    #[tokio::test]
    async fn test_lane_priority() {
        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        };
        let lane = Lane::new("test", config, priorities::SESSION);

        assert_eq!(lane.priority().await, priorities::SESSION);
    }

    #[tokio::test]
    async fn test_lane_status_initial() {
        let config = LaneConfig {
            min_concurrency: 2,
            max_concurrency: 8,
        };
        let lane = Lane::new("test", config, priorities::QUERY);

        let status = lane.status().await;
        assert_eq!(status.pending, 0);
        assert_eq!(status.active, 0);
        assert_eq!(status.min, 2);
        assert_eq!(status.max, 8);
    }

    #[tokio::test]
    async fn test_lane_enqueue() {
        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        };
        let lane = Lane::new("test", config, priorities::QUERY);

        let cmd = Box::new(TestCommand::new(serde_json::json!({"result": "ok"})));
        let _rx = lane.enqueue(cmd).await;

        let status = lane.status().await;
        assert_eq!(status.pending, 1);
    }

    #[tokio::test]
    async fn test_lane_status_serialization() {
        let status = LaneStatus {
            pending: 5,
            active: 2,
            min: 1,
            max: 8,
        };

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"pending\":5"));
        assert!(json.contains("\"active\":2"));
        assert!(json.contains("\"min\":1"));
        assert!(json.contains("\"max\":8"));

        let parsed: LaneStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pending, 5);
        assert_eq!(parsed.active, 2);
    }

    #[tokio::test]
    async fn test_command_queue_new() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        let status = queue.status().await;
        assert!(status.is_empty());
    }

    #[tokio::test]
    async fn test_command_queue_register_lane() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        };
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));

        queue.register_lane(lane).await;

        let status = queue.status().await;
        assert!(status.contains_key("test-lane"));
    }

    #[tokio::test]
    async fn test_command_queue_submit() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        };
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        let cmd = Box::new(TestCommand::new(serde_json::json!({"status": "ok"})));
        let result = queue.submit("test-lane", cmd).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_command_queue_submit_unknown_lane() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        let cmd = Box::new(TestCommand::new(serde_json::json!({})));
        let result = queue.submit("nonexistent", cmd).await;

        assert!(result.is_err());
        if let Err(LaneError::LaneNotFound(id)) = result {
            assert_eq!(id, "nonexistent");
        } else {
            panic!("Expected LaneNotFound error");
        }
    }

    #[tokio::test]
    async fn test_command_queue_multiple_lanes() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        // Register multiple lanes
        let configs = vec![
            ("system", priorities::SYSTEM, 1),
            ("control", priorities::CONTROL, 8),
            ("query", priorities::QUERY, 4),
        ];

        for (id, priority, max) in configs {
            let config = LaneConfig {
                min_concurrency: 1,
                max_concurrency: max,
            };
            let lane = Arc::new(Lane::new(id, config, priority));
            queue.register_lane(lane).await;
        }

        let status = queue.status().await;
        assert_eq!(status.len(), 3);
        assert!(status.contains_key("system"));
        assert!(status.contains_key("control"));
        assert!(status.contains_key("query"));
    }

    #[tokio::test]
    async fn test_command_queue_status() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        let config = LaneConfig {
            min_concurrency: 2,
            max_concurrency: 16,
        };
        let lane = Arc::new(Lane::new("query", config, priorities::QUERY));
        queue.register_lane(lane).await;

        let status = queue.status().await;
        let lane_status = status.get("query").unwrap();

        assert_eq!(lane_status.min, 2);
        assert_eq!(lane_status.max, 16);
        assert_eq!(lane_status.pending, 0);
        assert_eq!(lane_status.active, 0);
    }

    #[test]
    fn test_lane_state_has_capacity() {
        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 2,
        };
        let mut state = LaneState::new(config, priorities::QUERY);

        assert!(state.has_capacity());
        state.active = 1;
        assert!(state.has_capacity());
        state.active = 2;
        assert!(!state.has_capacity());
    }

    #[tokio::test]
    async fn test_lane_state_has_pending() {
        let config = LaneConfig {
            min_concurrency: 1,
            max_concurrency: 4,
        };
        let state = LaneState::new(config, priorities::QUERY);

        assert!(!state.has_pending());
        assert_eq!(state.pending.len(), 0);
    }

    #[test]
    fn test_lane_status_debug() {
        let status = LaneStatus {
            pending: 3,
            active: 1,
            min: 1,
            max: 4,
        };

        let debug_str = format!("{:?}", status);
        assert!(debug_str.contains("LaneStatus"));
        assert!(debug_str.contains("pending"));
        assert!(debug_str.contains("active"));
    }

    #[test]
    fn test_lane_status_clone() {
        let status = LaneStatus {
            pending: 5,
            active: 2,
            min: 1,
            max: 8,
        };

        let cloned = status.clone();
        assert_eq!(cloned.pending, 5);
        assert_eq!(cloned.active, 2);
        assert_eq!(cloned.min, 1);
        assert_eq!(cloned.max, 8);
    }

    #[tokio::test]
    async fn test_command_execution() {
        let cmd = TestCommand::new(serde_json::json!({"value": 42}));
        let result = cmd.execute().await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"value": 42}));
    }

    #[tokio::test]
    async fn test_command_type() {
        let cmd = TestCommand::new(serde_json::json!({}));
        assert_eq!(cmd.command_type(), "test");

        let failing = FailingCommand {
            message: "error".to_string(),
        };
        assert_eq!(failing.command_type(), "failing");
    }

    #[tokio::test]
    async fn test_failing_command() {
        let cmd = FailingCommand {
            message: "Something went wrong".to_string(),
        };

        let result = cmd.execute().await;
        assert!(result.is_err());

        if let Err(LaneError::Other(msg)) = result {
            assert_eq!(msg, "Something went wrong");
        } else {
            panic!("Expected Other error");
        }
    }

    #[tokio::test]
    async fn test_command_with_delay() {
        let cmd = TestCommand::with_delay(serde_json::json!({"delayed": true}), 10);

        let start = std::time::Instant::now();
        let result = cmd.execute().await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(elapsed.as_millis() >= 10);
    }

    #[test]
    fn test_priority_type() {
        let p: Priority = 5;
        assert_eq!(p, 5u8);
    }

    #[test]
    fn test_lane_id_type() {
        let id: LaneId = "test-lane".to_string();
        assert_eq!(id, "test-lane");
    }

    #[test]
    fn test_command_id_type() {
        let id: CommandId = "cmd-123".to_string();
        assert_eq!(id, "cmd-123");
    }
}
