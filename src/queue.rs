//! Core queue implementation with lanes and priority scheduling

#[cfg(feature = "distributed")]
use crate::boost::PriorityBooster;
use crate::config::LaneConfig;
use crate::dlq::{DeadLetter, DeadLetterQueue};
use crate::error::{LaneError, Result};
use crate::event::{events, EventEmitter, EventStream, LaneEvent};
#[cfg(feature = "distributed")]
use crate::ratelimit::RateLimiter;
use crate::retry::RetryPolicy;
use crate::storage::{Storage, StoredCommand};
#[cfg(feature = "telemetry")]
use crate::telemetry;
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(feature = "telemetry")]
use std::time::Instant;
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

/// A simple JSON-based command for SDK usage.
///
/// Returns the payload as-is when executed. Useful for language bindings
/// (Python, Node.js) where commands are represented as JSON data.
pub struct JsonCommand {
    command_type: String,
    payload: serde_json::Value,
}

impl JsonCommand {
    /// Create a new JSON command.
    pub fn new(command_type: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            command_type: command_type.into(),
            payload,
        }
    }
}

#[async_trait]
impl Command for JsonCommand {
    async fn execute(&self) -> Result<serde_json::Value> {
        Ok(self.payload.clone())
    }

    fn command_type(&self) -> &str {
        &self.command_type
    }
}

/// Command wrapper
struct CommandWrapper {
    id: CommandId,
    command: Arc<dyn Command>,
    result_tx: Option<tokio::sync::oneshot::Sender<Result<serde_json::Value>>>,
    timeout: Option<std::time::Duration>,
    retry_policy: RetryPolicy,
    attempt: u32,
    lane_id: LaneId,
    command_type: String,
    /// Submission time used by the priority booster to calculate deadline proximity
    #[cfg(feature = "distributed")]
    enqueue_time: chrono::DateTime<chrono::Utc>,
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

    /// True when the lane is currently considered under pressure
    is_pressured: bool,
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
            is_pressured: false,
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
    storage: Option<Arc<dyn Storage>>,
    /// Rate limiter instantiated from LaneConfig.rate_limit (None = unlimited)
    #[cfg(feature = "distributed")]
    rate_limiter: RateLimiter,
    /// Priority booster instantiated from LaneConfig.priority_boost
    #[cfg(feature = "distributed")]
    booster: Option<PriorityBooster>,
}

impl Lane {
    /// Create a new lane
    pub fn new(id: impl Into<String>, config: LaneConfig, priority: Priority) -> Self {
        #[cfg(feature = "distributed")]
        let rate_limiter = config
            .rate_limit
            .as_ref()
            .map(RateLimiter::token_bucket)
            .unwrap_or_default();
        #[cfg(feature = "distributed")]
        let booster = config
            .priority_boost
            .as_ref()
            .map(|pb| PriorityBooster::new(pb.clone()));
        Self {
            id: id.into(),
            state: Arc::new(Mutex::new(LaneState::new(config, priority))),
            storage: None,
            #[cfg(feature = "distributed")]
            rate_limiter,
            #[cfg(feature = "distributed")]
            booster,
        }
    }

    /// Create a new lane with storage
    pub fn with_storage(
        id: impl Into<String>,
        config: LaneConfig,
        priority: Priority,
        storage: Arc<dyn Storage>,
    ) -> Self {
        #[cfg(feature = "distributed")]
        let rate_limiter = config
            .rate_limit
            .as_ref()
            .map(RateLimiter::token_bucket)
            .unwrap_or_default();
        #[cfg(feature = "distributed")]
        let booster = config
            .priority_boost
            .as_ref()
            .map(|pb| PriorityBooster::new(pb.clone()));
        Self {
            id: id.into(),
            state: Arc::new(Mutex::new(LaneState::new(config, priority))),
            storage: Some(storage),
            #[cfg(feature = "distributed")]
            rate_limiter,
            #[cfg(feature = "distributed")]
            booster,
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

    /// Get effective priority, applying boost based on the front command's age
    pub async fn effective_priority(&self) -> Priority {
        let state = self.state.lock().await;
        let base = state.priority;
        #[cfg(feature = "distributed")]
        if let Some(booster) = &self.booster {
            if let Some(front) = state.pending.front() {
                return booster.calculate_priority(base, front.enqueue_time);
            }
        }
        base
    }

    /// Enqueue a command
    pub async fn enqueue(
        &self,
        command: Box<dyn Command>,
    ) -> tokio::sync::oneshot::Receiver<Result<serde_json::Value>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let state = self.state.lock().await;
        let timeout = state.config.default_timeout;
        let retry_policy = state.config.retry_policy.clone();
        drop(state);

        let command_id = Uuid::new_v4().to_string();
        let command_type = command.command_type().to_string();
        let wrapper = CommandWrapper {
            id: command_id.clone(),
            command: Arc::from(command),
            result_tx: Some(tx),
            timeout,
            retry_policy,
            attempt: 0,
            lane_id: self.id.clone(),
            command_type: command_type.clone(),
            #[cfg(feature = "distributed")]
            enqueue_time: Utc::now(),
        };

        // Persist to storage if available
        if let Some(storage) = &self.storage {
            let stored_cmd = StoredCommand {
                id: command_id,
                command_type,
                lane_id: self.id.clone(),
                payload: serde_json::json!({}), // Empty payload for now
                retry_count: 0,
                created_at: Utc::now(),
                last_attempt_at: None,
            };
            // Ignore storage errors to not block command execution
            let _ = storage.save_command(stored_cmd).await;
        }

        let mut state = self.state.lock().await;
        state.pending.push_back(wrapper);

        rx
    }

    /// Re-enqueue a command for retry (internal use)
    async fn retry_command(&self, mut wrapper: CommandWrapper, delay: std::time::Duration) {
        wrapper.attempt += 1;

        // Spawn a task to re-enqueue after delay
        let state_clone = Arc::clone(&self.state);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let mut state = state_clone.lock().await;
            state.pending.push_back(wrapper);
        });
    }

    /// Try to dequeue a command for execution
    async fn try_dequeue(&self) -> Option<CommandWrapper> {
        // Check rate limiter before acquiring the state lock
        #[cfg(feature = "distributed")]
        if !self.rate_limiter.try_acquire().await {
            return None;
        }
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

    /// Check for pressure state transitions.
    ///
    /// Returns the event key to emit if a transition occurred, or `None` if no change.
    /// - Transitions to pressured when `pending >= threshold` and was not already pressured.
    /// - Transitions to idle when `pending == 0` and was previously pressured.
    async fn check_pressure(&self) -> Option<&'static str> {
        let mut state = self.state.lock().await;
        let threshold = match state.config.pressure_threshold {
            Some(t) => t,
            None => return None,
        };
        let pending = state.pending.len();
        let was_pressured = state.is_pressured;
        if pending >= threshold && !was_pressured {
            state.is_pressured = true;
            Some(events::QUEUE_LANE_PRESSURE)
        } else if pending == 0 && was_pressured {
            state.is_pressured = false;
            Some(events::QUEUE_LANE_IDLE)
        } else {
            None
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
    dlq: Option<DeadLetterQueue>,
    storage: Option<Arc<dyn Storage>>,
    is_shutting_down: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
}

impl CommandQueue {
    /// Create a new command queue
    pub fn new(event_emitter: EventEmitter) -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
            dlq: None,
            storage: None,
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Create a new command queue with a dead letter queue
    pub fn with_dlq(event_emitter: EventEmitter, dlq_size: usize) -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
            dlq: Some(DeadLetterQueue::new(dlq_size)),
            storage: None,
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Create a new command queue with storage
    pub fn with_storage(event_emitter: EventEmitter, storage: Arc<dyn Storage>) -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
            dlq: None,
            storage: Some(storage),
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Create a new command queue with both DLQ and storage
    pub fn with_dlq_and_storage(
        event_emitter: EventEmitter,
        dlq_size: usize,
        storage: Arc<dyn Storage>,
    ) -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
            dlq: Some(DeadLetterQueue::new(dlq_size)),
            storage: Some(storage),
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Get the storage backend
    pub fn storage(&self) -> Option<&Arc<dyn Storage>> {
        self.storage.as_ref()
    }

    /// Get the dead letter queue
    pub fn dlq(&self) -> Option<&DeadLetterQueue> {
        self.dlq.as_ref()
    }

    /// Check if shutdown is in progress
    pub fn is_shutting_down(&self) -> bool {
        self.is_shutting_down.load(Ordering::SeqCst)
    }

    /// Initiate graceful shutdown - stop accepting new commands
    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        self.event_emitter
            .emit(LaneEvent::empty(events::QUEUE_SHUTDOWN_STARTED));
    }

    /// Wait for all pending commands to complete (with timeout)
    pub async fn drain(&self, timeout: std::time::Duration) -> Result<()> {
        let start = std::time::Instant::now();

        loop {
            // Check if all lanes are empty and idle
            let lanes = self.lanes.lock().await;
            let mut all_idle = true;

            for lane in lanes.values() {
                let status = lane.status().await;
                if status.pending > 0 || status.active > 0 {
                    all_idle = false;
                    break;
                }
            }
            drop(lanes);

            if all_idle {
                return Ok(());
            }

            // Check timeout
            if start.elapsed() >= timeout {
                return Err(LaneError::Timeout(timeout));
            }

            // Wait a bit before checking again
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
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
        // Reject new commands during shutdown
        if self.is_shutting_down() {
            return Err(LaneError::ShutdownInProgress);
        }

        let lanes = self.lanes.lock().await;
        let lane = lanes
            .get(lane_id)
            .ok_or_else(|| LaneError::LaneNotFound(lane_id.to_string()))?;
        let rx = lane.enqueue(command).await;

        self.event_emitter.emit(LaneEvent::with_map(
            events::QUEUE_COMMAND_SUBMITTED,
            HashMap::from([("lane_id".to_string(), serde_json::json!(lane_id))]),
        ));

        Ok(rx)
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
        // Find the highest-priority lane with pending commands.
        // effective_priority applies any deadline-based boost configured on the lane.
        let lanes = self.lanes.lock().await;

        // Check pressure transitions for all lanes and emit events
        for (lane_id, lane) in lanes.iter() {
            if let Some(event_key) = lane.check_pressure().await {
                self.event_emitter.emit(LaneEvent::with_map(
                    event_key,
                    HashMap::from([("lane_id".to_string(), serde_json::json!(lane_id))]),
                ));
            }
        }

        let mut lane_priorities = Vec::new();
        for lane in lanes.values() {
            let priority = lane.effective_priority().await;
            lane_priorities.push((priority, Arc::clone(lane)));
        }

        // Sort by priority (lower number = higher priority)
        lane_priorities.sort_by_key(|(priority, _)| *priority);

        for (_, lane) in lane_priorities {
            if let Some(mut wrapper) = lane.try_dequeue().await {
                let lane_clone = Arc::clone(&lane);
                let timeout = wrapper.timeout;
                let retry_policy = wrapper.retry_policy.clone();
                let attempt = wrapper.attempt;
                let dlq = self.dlq.clone();
                let command_id = wrapper.id.clone();
                let command_type = wrapper.command_type.clone();
                let lane_id = wrapper.lane_id.clone();
                let storage = lane.storage.clone();
                let event_emitter = self.event_emitter.clone();

                event_emitter.emit(LaneEvent::with_map(
                    events::QUEUE_COMMAND_STARTED,
                    HashMap::from([
                        ("lane_id".to_string(), serde_json::json!(lane_id)),
                        ("command_id".to_string(), serde_json::json!(command_id)),
                        ("command_type".to_string(), serde_json::json!(command_type)),
                    ]),
                ));

                tokio::spawn(async move {
                    #[cfg(feature = "telemetry")]
                    let exec_start = Instant::now();

                    let result = match timeout {
                        Some(dur) => {
                            match tokio::time::timeout(dur, wrapper.command.execute()).await {
                                Ok(r) => r,
                                Err(_) => Err(LaneError::Timeout(dur)),
                            }
                        }
                        None => wrapper.command.execute().await,
                    };

                    match result {
                        Ok(value) => {
                            if let Some(storage) = &storage {
                                let _ = storage.remove_command(&command_id).await;
                            }

                            #[cfg(feature = "telemetry")]
                            telemetry::record_complete(
                                &lane_id,
                                exec_start.elapsed().as_secs_f64(),
                            );

                            event_emitter.emit(LaneEvent::with_map(
                                events::QUEUE_COMMAND_COMPLETED,
                                HashMap::from([
                                    ("lane_id".to_string(), serde_json::json!(lane_id)),
                                    ("command_id".to_string(), serde_json::json!(command_id)),
                                ]),
                            ));

                            if let Some(tx) = wrapper.result_tx.take() {
                                let _ = tx.send(Ok(value));
                            }
                            lane_clone.mark_completed().await;
                        }
                        Err(err) => {
                            if retry_policy.should_retry(attempt) {
                                let delay = retry_policy.delay_for_attempt(attempt + 1);

                                tracing::info!(
                                    command_id = %command_id,
                                    retry_attempt = attempt + 1,
                                    "a3s.lane.retry: retrying command"
                                );

                                event_emitter.emit(LaneEvent::with_map(
                                    events::QUEUE_COMMAND_RETRY,
                                    HashMap::from([
                                        ("lane_id".to_string(), serde_json::json!(lane_id)),
                                        ("command_id".to_string(), serde_json::json!(command_id)),
                                        ("attempt".to_string(), serde_json::json!(attempt + 1)),
                                    ]),
                                ));

                                lane_clone.retry_command(wrapper, delay).await;
                                lane_clone.mark_completed().await;
                            } else {
                                #[cfg(feature = "telemetry")]
                                telemetry::record_failure(&lane_id);

                                if let Some(storage) = &storage {
                                    let _ = storage.remove_command(&command_id).await;
                                }

                                let error_msg = err.to_string();
                                let is_timeout = matches!(err, LaneError::Timeout(_));

                                if let Some(dlq) = dlq {
                                    let dead_letter = DeadLetter {
                                        command_id: command_id.clone(),
                                        command_type: command_type.clone(),
                                        lane_id: lane_id.clone(),
                                        error: error_msg.clone(),
                                        attempts: attempt + 1,
                                        failed_at: Utc::now(),
                                    };
                                    dlq.push(dead_letter).await;

                                    event_emitter.emit(LaneEvent::with_map(
                                        events::QUEUE_COMMAND_DEAD_LETTERED,
                                        HashMap::from([
                                            ("lane_id".to_string(), serde_json::json!(lane_id)),
                                            (
                                                "command_id".to_string(),
                                                serde_json::json!(command_id),
                                            ),
                                            (
                                                "command_type".to_string(),
                                                serde_json::json!(command_type),
                                            ),
                                        ]),
                                    ));
                                }

                                event_emitter.emit(LaneEvent::with_map(
                                    if is_timeout {
                                        events::QUEUE_COMMAND_TIMEOUT
                                    } else {
                                        events::QUEUE_COMMAND_FAILED
                                    },
                                    HashMap::from([
                                        ("lane_id".to_string(), serde_json::json!(lane_id)),
                                        ("command_id".to_string(), serde_json::json!(command_id)),
                                        ("error".to_string(), serde_json::json!(error_msg)),
                                    ]),
                                ));

                                if let Some(tx) = wrapper.result_tx.take() {
                                    let _ = tx.send(Err(err));
                                }
                                lane_clone.mark_completed().await;
                            }
                        }
                    }
                });
                break;
            }
        }
    }

    /// Subscribe to all queue lifecycle events as an `EventStream` (implements `Stream`)
    pub fn subscribe_stream(&self) -> EventStream {
        self.event_emitter.subscribe_stream()
    }

    /// Subscribe to filtered queue lifecycle events as an `EventStream`
    pub fn subscribe_filtered(
        &self,
        filter: impl Fn(&LaneEvent) -> bool + Send + Sync + 'static,
    ) -> EventStream {
        self.event_emitter.subscribe_filtered(filter)
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
        let config = LaneConfig::new(1, 4);
        let lane = Lane::new("test-lane", config, priorities::QUERY);

        assert_eq!(lane.id(), "test-lane");
    }

    #[tokio::test]
    async fn test_lane_priority() {
        let config = LaneConfig::new(1, 4);
        let lane = Lane::new("test", config, priorities::SESSION);

        assert_eq!(lane.priority().await, priorities::SESSION);
    }

    #[tokio::test]
    async fn test_lane_status_initial() {
        let config = LaneConfig::new(2, 8);
        let lane = Lane::new("test", config, priorities::QUERY);

        let status = lane.status().await;
        assert_eq!(status.pending, 0);
        assert_eq!(status.active, 0);
        assert_eq!(status.min, 2);
        assert_eq!(status.max, 8);
    }

    #[tokio::test]
    async fn test_lane_enqueue() {
        let config = LaneConfig::new(1, 4);
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

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));

        queue.register_lane(lane).await;

        let status = queue.status().await;
        assert!(status.contains_key("test-lane"));
    }

    #[tokio::test]
    async fn test_command_queue_submit() {
        let emitter = EventEmitter::new(100);
        let queue = CommandQueue::new(emitter);

        let config = LaneConfig::new(1, 4);
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
            let config = LaneConfig::new(1, max);
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

        let config = LaneConfig::new(2, 16);
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
        let config = LaneConfig::new(1, 2);
        let mut state = LaneState::new(config, priorities::QUERY);

        assert!(state.has_capacity());
        state.active = 1;
        assert!(state.has_capacity());
        state.active = 2;
        assert!(!state.has_capacity());
    }

    #[tokio::test]
    async fn test_lane_state_has_pending() {
        let config = LaneConfig::new(1, 4);
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

    #[tokio::test]
    async fn test_command_timeout() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let config = LaneConfig::new(1, 4).with_timeout(std::time::Duration::from_millis(50));
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        // Start scheduler
        Arc::clone(&queue).start_scheduler().await;

        // Submit a command that takes longer than timeout
        let cmd = Box::new(TestCommand::with_delay(
            serde_json::json!({"result": "ok"}),
            200,
        ));
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        // Wait for result
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        // Should be a timeout error
        assert!(result.is_err());
        if let Err(LaneError::Timeout(dur)) = result {
            assert_eq!(dur, std::time::Duration::from_millis(50));
        } else {
            panic!("Expected Timeout error");
        }
    }

    #[tokio::test]
    async fn test_command_no_timeout() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        // Submit a command with delay but no timeout configured
        let cmd = Box::new(TestCommand::with_delay(
            serde_json::json!({"result": "ok"}),
            50,
        ));
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        // Should succeed
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["result"], "ok");
    }

    #[tokio::test]
    async fn test_command_completes_before_timeout() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let config = LaneConfig::new(1, 4).with_timeout(std::time::Duration::from_secs(5));
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        // Submit a fast command with long timeout
        let cmd = Box::new(TestCommand::with_delay(
            serde_json::json!({"result": "fast"}),
            10,
        ));
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        // Should succeed
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["result"], "fast");
    }

    #[tokio::test]
    async fn test_command_retry_on_failure() {
        use std::sync::atomic::{AtomicU32, Ordering};

        // Command that fails first 2 times, then succeeds
        struct RetryableCommand {
            attempts: Arc<AtomicU32>,
        }

        #[async_trait]
        impl Command for RetryableCommand {
            async fn execute(&self) -> Result<serde_json::Value> {
                let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err(LaneError::Other(format!("Attempt {} failed", attempt)))
                } else {
                    Ok(serde_json::json!({"success": true, "attempts": attempt + 1}))
                }
            }

            fn command_type(&self) -> &str {
                "retryable"
            }
        }

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let retry_policy = RetryPolicy::fixed(3, std::time::Duration::from_millis(10));
        let config = LaneConfig::new(1, 4).with_retry_policy(retry_policy);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        let attempts = Arc::new(AtomicU32::new(0));
        let cmd = Box::new(RetryableCommand {
            attempts: Arc::clone(&attempts),
        });
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        // Wait for result (should succeed after retries)
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        assert!(result.is_ok());
        let value = result.unwrap();
        assert_eq!(value["success"], true);
        assert_eq!(value["attempts"], 3); // Failed twice, succeeded on 3rd attempt
    }

    #[tokio::test]
    async fn test_command_retry_exhausted() {
        // Command that always fails
        struct AlwaysFailCommand;

        #[async_trait]
        impl Command for AlwaysFailCommand {
            async fn execute(&self) -> Result<serde_json::Value> {
                Err(LaneError::Other("Always fails".to_string()))
            }

            fn command_type(&self) -> &str {
                "always_fail"
            }
        }

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let retry_policy = RetryPolicy::fixed(2, std::time::Duration::from_millis(10));
        let config = LaneConfig::new(1, 4).with_retry_policy(retry_policy);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        let cmd = Box::new(AlwaysFailCommand);
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        // Wait for result (should fail after exhausting retries)
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        assert!(result.is_err());
        if let Err(LaneError::Other(msg)) = result {
            assert_eq!(msg, "Always fails");
        } else {
            panic!("Expected Other error");
        }
    }

    #[tokio::test]
    async fn test_command_no_retry_on_success() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct CountingCommand {
            counter: Arc<AtomicU32>,
        }

        #[async_trait]
        impl Command for CountingCommand {
            async fn execute(&self) -> Result<serde_json::Value> {
                let count = self.counter.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({"count": count + 1}))
            }

            fn command_type(&self) -> &str {
                "counting"
            }
        }

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let retry_policy = RetryPolicy::exponential(3);
        let config = LaneConfig::new(1, 4).with_retry_policy(retry_policy);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        let counter = Arc::new(AtomicU32::new(0));
        let cmd = Box::new(CountingCommand {
            counter: Arc::clone(&counter),
        });
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        assert!(result.is_ok());
        assert_eq!(result.unwrap()["count"], 1);

        // Verify command was only executed once (no retries on success)
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_dlq_integration() {
        // Command that always fails
        struct FailCommand;

        #[async_trait]
        impl Command for FailCommand {
            async fn execute(&self) -> Result<serde_json::Value> {
                Err(LaneError::Other("Permanent failure".to_string()))
            }

            fn command_type(&self) -> &str {
                "fail_command"
            }
        }

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::with_dlq(emitter, 100));

        let retry_policy = RetryPolicy::fixed(2, std::time::Duration::from_millis(10));
        let config = LaneConfig::new(1, 4).with_retry_policy(retry_policy);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        // Submit a failing command
        let cmd = Box::new(FailCommand);
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        // Wait for result
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("Timeout waiting for result")
            .expect("Channel closed");

        assert!(result.is_err());

        // Check DLQ
        let dlq = queue.dlq().expect("DLQ should exist");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await; // Give time for DLQ push

        assert_eq!(dlq.len().await, 1);

        let letters = dlq.list().await;
        assert_eq!(letters[0].command_type, "fail_command");
        assert_eq!(letters[0].lane_id, "test-lane");
        assert_eq!(letters[0].attempts, 3); // Initial + 2 retries
        assert!(letters[0].error.contains("Permanent failure"));
    }

    #[tokio::test]
    async fn test_no_dlq_without_configuration() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        assert!(queue.dlq().is_none());
    }

    #[tokio::test]
    async fn test_shutdown_rejects_new_commands() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        // Initiate shutdown
        queue.shutdown().await;
        assert!(queue.is_shutting_down());

        // Try to submit a command - should be rejected
        let cmd = Box::new(TestCommand::new(serde_json::json!({"test": "data"})));
        let result = queue.submit("test-lane", cmd).await;

        assert!(result.is_err());
        if let Err(LaneError::ShutdownInProgress) = result {
            // Expected
        } else {
            panic!("Expected ShutdownInProgress error");
        }
    }

    #[tokio::test]
    async fn test_drain_waits_for_completion() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        // Submit a slow command
        let cmd = Box::new(TestCommand::with_delay(
            serde_json::json!({"result": "ok"}),
            100,
        ));
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        // Initiate shutdown
        queue.shutdown().await;

        // Drain should wait for the command to complete
        let drain_result = queue.drain(std::time::Duration::from_secs(2)).await;
        assert!(drain_result.is_ok());

        // Command should have completed
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_timeout() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        Arc::clone(&queue).start_scheduler().await;

        // Submit a very slow command
        let cmd = Box::new(TestCommand::with_delay(
            serde_json::json!({"result": "ok"}),
            5000,
        ));
        let _rx = queue.submit("test-lane", cmd).await.unwrap();

        // Initiate shutdown
        queue.shutdown().await;

        // Drain with short timeout should fail
        let drain_result = queue.drain(std::time::Duration::from_millis(50)).await;
        assert!(drain_result.is_err());
        if let Err(LaneError::Timeout(_)) = drain_result {
            // Expected
        } else {
            panic!("Expected Timeout error");
        }
    }

    #[tokio::test]
    async fn test_is_shutting_down() {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        assert!(!queue.is_shutting_down());

        queue.shutdown().await;
        assert!(queue.is_shutting_down());
    }

    // ── Pressure event tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_lane_pressure_emits_on_threshold() {
        use crate::event::events;

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter.clone()));

        let config = LaneConfig::new(1, 4).with_pressure_threshold(2);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        // Subscribe before enqueuing to capture all events
        let mut stream = emitter.subscribe_filtered(|e| e.key == events::QUEUE_LANE_PRESSURE);

        // Enqueue 2 commands (meets threshold=2)
        for _ in 0..2 {
            let cmd = Box::new(TestCommand::new(serde_json::json!({})));
            let _ = queue.submit("test-lane", cmd).await.unwrap();
        }

        // Start scheduler — first tick calls check_pressure → pending=2 >= 2 → emit PRESSURE
        Arc::clone(&queue).start_scheduler().await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), stream.recv())
            .await
            .expect("No pressure event received within timeout")
            .expect("Stream ended");

        assert_eq!(event.key, events::QUEUE_LANE_PRESSURE);
    }

    #[tokio::test]
    async fn test_lane_idle_emits_when_drained() {
        use crate::event::events;

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter.clone()));

        let config = LaneConfig::new(1, 4).with_pressure_threshold(1);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        // Subscribe to both pressure and idle events
        let mut stream = emitter.subscribe_filtered(|e| {
            e.key == events::QUEUE_LANE_PRESSURE || e.key == events::QUEUE_LANE_IDLE
        });

        // Enqueue 1 command (meets threshold=1)
        let cmd = Box::new(TestCommand::new(serde_json::json!({})));
        let _ = queue.submit("test-lane", cmd).await.unwrap();

        // Start scheduler
        Arc::clone(&queue).start_scheduler().await;

        // First event must be pressure
        let pressure = tokio::time::timeout(std::time::Duration::from_secs(1), stream.recv())
            .await
            .expect("No pressure event")
            .expect("Stream ended");
        assert_eq!(pressure.key, events::QUEUE_LANE_PRESSURE);

        // After dequeue, pending=0 → idle event on the next scheduler tick
        let idle = tokio::time::timeout(std::time::Duration::from_secs(1), stream.recv())
            .await
            .expect("No idle event")
            .expect("Stream ended");
        assert_eq!(idle.key, events::QUEUE_LANE_IDLE);
    }

    #[tokio::test]
    async fn test_lane_no_pressure_without_threshold() {
        use crate::event::events;

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter.clone()));

        // No pressure threshold
        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        let mut stream = emitter.subscribe_filtered(|e| {
            e.key == events::QUEUE_LANE_PRESSURE || e.key == events::QUEUE_LANE_IDLE
        });

        // Enqueue several commands
        for _ in 0..5 {
            let cmd = Box::new(TestCommand::new(serde_json::json!({})));
            let _ = queue.submit("test-lane", cmd).await.unwrap();
        }

        Arc::clone(&queue).start_scheduler().await;

        // Allow time for scheduler ticks to run
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // No pressure/idle events should have been emitted
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), stream.recv()).await;

        assert!(
            result.is_err(),
            "Should not receive pressure/idle events without threshold"
        );
    }

    // ── Bug-fix tests ──────────────────────────────────────────────────────────

    /// Bug fix: EventEmitter.emit() is now called on submit, start, complete, retry,
    /// dead-letter, fail, and shutdown.
    #[tokio::test]
    async fn test_event_emitted_on_submit() {
        use crate::event::events;

        let emitter = EventEmitter::new(100);
        let mut rx = emitter.subscribe();
        let queue = CommandQueue::new(emitter);

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;

        let cmd = Box::new(TestCommand::new(serde_json::json!({"result": "ok"})));
        let _ = queue.submit("test-lane", cmd).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            rx.recv().await.unwrap()
        })
        .await
        .expect("QUEUE_COMMAND_SUBMITTED event not received");

        assert_eq!(event.key, events::QUEUE_COMMAND_SUBMITTED);
    }

    #[tokio::test]
    async fn test_event_emitted_on_complete() {
        use crate::event::{events, EventStream};

        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter.clone()));

        let config = LaneConfig::new(1, 4);
        let lane = Arc::new(Lane::new("test-lane", config, priorities::QUERY));
        queue.register_lane(lane).await;
        Arc::clone(&queue).start_scheduler().await;

        let mut stream: EventStream =
            emitter.subscribe_filtered(|e| e.key == events::QUEUE_COMMAND_COMPLETED);

        let cmd = Box::new(TestCommand::new(serde_json::json!({"result": "ok"})));
        let rx = queue.submit("test-lane", cmd).await.unwrap();

        // Wait for command to complete
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;

        let event = tokio::time::timeout(std::time::Duration::from_millis(200), stream.recv())
            .await
            .expect("QUEUE_COMMAND_COMPLETED event not received")
            .expect("stream closed");

        assert_eq!(event.key, events::QUEUE_COMMAND_COMPLETED);
    }

    #[tokio::test]
    async fn test_event_emitted_on_shutdown() {
        use crate::event::events;

        let emitter = EventEmitter::new(100);
        let mut rx = emitter.subscribe();
        let queue = CommandQueue::new(emitter);

        queue.shutdown().await;

        let event = tokio::time::timeout(std::time::Duration::from_millis(100), async {
            rx.recv().await.unwrap()
        })
        .await
        .expect("QUEUE_SHUTDOWN_STARTED event not received");

        assert_eq!(event.key, events::QUEUE_SHUTDOWN_STARTED);
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn test_rate_limit_blocks_dequeue() {
        use crate::ratelimit::RateLimitConfig;

        // Token bucket starts full (1 token for per_second(1))
        let config = LaneConfig::new(1, 10).with_rate_limit(RateLimitConfig::per_second(1));
        let lane = Lane::new("test", config, priorities::QUERY);

        let _rx1 = lane
            .enqueue(Box::new(TestCommand::new(serde_json::json!(1))))
            .await;
        let _rx2 = lane
            .enqueue(Box::new(TestCommand::new(serde_json::json!(2))))
            .await;

        // First dequeue consumes the single available token
        let first = lane.try_dequeue().await;
        assert!(first.is_some(), "first dequeue should succeed");
        // Return the slot so capacity isn't the limiting factor
        lane.mark_completed().await;

        // No tokens left — rate limiter should block the second dequeue
        let second = lane.try_dequeue().await;
        assert!(second.is_none(), "second dequeue should be rate-limited");
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn test_effective_priority_no_boost_when_fresh() {
        use crate::boost::PriorityBoostConfig;

        // Boost activates with 9 s remaining on a 10 s deadline.
        // A freshly-enqueued command has ~10 s left, so no boost yet.
        let config = LaneConfig::new(1, 4).with_priority_boost(
            PriorityBoostConfig::new(std::time::Duration::from_secs(10))
                .with_boost(std::time::Duration::from_secs(9), 2),
        );
        let lane = Lane::new("test", config, priorities::QUERY);

        // Without any pending command the lane returns its base priority
        assert_eq!(lane.effective_priority().await, priorities::QUERY);

        // A just-enqueued command has ~10 s left — no boost
        let _rx = lane
            .enqueue(Box::new(TestCommand::new(serde_json::json!(1))))
            .await;
        assert_eq!(lane.effective_priority().await, priorities::QUERY);
    }

    #[cfg(feature = "distributed")]
    #[tokio::test]
    async fn test_effective_priority_boosted_when_past_deadline() {
        use crate::boost::PriorityBoostConfig;

        // Deadline already passed — effective priority should reach 0 (maximum)
        let config = LaneConfig::new(1, 4).with_priority_boost(PriorityBoostConfig::standard(
            std::time::Duration::from_millis(1), // 1 ms deadline
        ));
        let lane = Lane::new("test", config, priorities::PROMPT); // base = 5

        let _rx = lane
            .enqueue(Box::new(TestCommand::new(serde_json::json!(1))))
            .await;

        // Sleep past the deadline so the booster gives priority 0
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert_eq!(lane.effective_priority().await, 0);
    }
}
