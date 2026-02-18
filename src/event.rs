//! Event system for queue notifications

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::broadcast;

/// Event key type
pub type EventKey = String;

/// Event payload
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventPayload {
    Empty,
    String(String),
    Map(HashMap<String, serde_json::Value>),
}

/// Lane event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneEvent {
    /// Event key (e.g., "queue.lane.pressure", "queue.command.completed")
    pub key: EventKey,

    /// Event payload
    pub payload: EventPayload,

    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl LaneEvent {
    /// Create a new event
    pub fn new(key: impl Into<String>, payload: EventPayload) -> Self {
        Self {
            key: key.into(),
            payload,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Create an event with no payload
    pub fn empty(key: impl Into<String>) -> Self {
        Self::new(key, EventPayload::Empty)
    }

    /// Create an event with a string payload
    pub fn with_string(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(key, EventPayload::String(message.into()))
    }

    /// Create an event with a map payload
    pub fn with_map(key: impl Into<String>, map: HashMap<String, serde_json::Value>) -> Self {
        Self::new(key, EventPayload::Map(map))
    }
}

/// Event emitter
#[derive(Clone)]
pub struct EventEmitter {
    sender: Arc<broadcast::Sender<LaneEvent>>,
}

impl EventEmitter {
    /// Create a new event emitter
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender: Arc::new(sender),
        }
    }

    /// Emit an event
    pub fn emit(&self, event: LaneEvent) {
        let _ = self.sender.send(event);
    }

    /// Subscribe to events
    pub fn subscribe(&self) -> broadcast::Receiver<LaneEvent> {
        self.sender.subscribe()
    }

    /// Subscribe to filtered events as an `EventStream` (implements `Stream`)
    pub fn subscribe_filtered(
        &self,
        filter: impl Fn(&LaneEvent) -> bool + Send + Sync + 'static,
    ) -> EventStream {
        use tokio_stream::wrappers::BroadcastStream;
        use tokio_stream::StreamExt as TokioStreamExt;
        let stream = BroadcastStream::new(self.sender.subscribe())
            .filter_map(|r: Result<LaneEvent, _>| r.ok())
            .filter(move |e| filter(e));
        EventStream {
            inner: Box::pin(stream),
        }
    }

    /// Subscribe to all events as an `EventStream` (implements `Stream`)
    pub fn subscribe_stream(&self) -> EventStream {
        self.subscribe_filtered(|_| true)
    }
}

/// Event stream — implements `futures_core::Stream<Item = LaneEvent>`.
///
/// Returned by [`EventEmitter::subscribe_filtered`] and [`EventEmitter::subscribe_stream`].
/// Use `.next().await` via `StreamExt` from `tokio_stream` or `futures`, or call the
/// convenience [`EventStream::recv`] method directly.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = LaneEvent> + Send>>,
}

impl Stream for EventStream {
    type Item = LaneEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl EventStream {
    /// Receive the next matching event (convenience wrapper around `Stream::poll_next`)
    pub async fn recv(&mut self) -> Option<LaneEvent> {
        use tokio_stream::StreamExt;
        self.next().await
    }
}

/// Event catalog - predefined event keys
pub mod events {
    // Queue events
    pub const QUEUE_LANE_PRESSURE: &str = "queue.lane.pressure";
    pub const QUEUE_LANE_IDLE: &str = "queue.lane.idle";
    pub const QUEUE_COMMAND_SUBMITTED: &str = "queue.command.submitted";
    pub const QUEUE_COMMAND_STARTED: &str = "queue.command.started";
    pub const QUEUE_COMMAND_COMPLETED: &str = "queue.command.completed";
    pub const QUEUE_COMMAND_FAILED: &str = "queue.command.failed";
    pub const QUEUE_COMMAND_TIMEOUT: &str = "queue.command.timeout";
    pub const QUEUE_COMMAND_RETRY: &str = "queue.command.retry";
    pub const QUEUE_COMMAND_DEAD_LETTERED: &str = "queue.command.dead_lettered";
    pub const QUEUE_SHUTDOWN_STARTED: &str = "queue.shutdown.started";
    pub const QUEUE_SHUTDOWN_COMPLETE: &str = "queue.shutdown.complete";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lane_event_new() {
        let event = LaneEvent::new("test.event", EventPayload::Empty);

        assert_eq!(event.key, "test.event");
        assert!(matches!(event.payload, EventPayload::Empty));
    }

    #[test]
    fn test_lane_event_empty() {
        let event = LaneEvent::empty("queue.ready");

        assert_eq!(event.key, "queue.ready");
        assert!(matches!(event.payload, EventPayload::Empty));
    }

    #[test]
    fn test_lane_event_with_string() {
        let event = LaneEvent::with_string("queue.error", "Connection lost");

        assert_eq!(event.key, "queue.error");
        if let EventPayload::String(msg) = &event.payload {
            assert_eq!(msg, "Connection lost");
        } else {
            panic!("Expected string payload");
        }
    }

    #[test]
    fn test_lane_event_with_map() {
        let mut map = HashMap::new();
        map.insert("lane_id".to_string(), serde_json::json!("query"));
        map.insert("pending".to_string(), serde_json::json!(10));

        let event = LaneEvent::with_map("queue.status", map);

        assert_eq!(event.key, "queue.status");
        if let EventPayload::Map(m) = &event.payload {
            assert_eq!(m.get("lane_id").unwrap(), &serde_json::json!("query"));
            assert_eq!(m.get("pending").unwrap(), &serde_json::json!(10));
        } else {
            panic!("Expected map payload");
        }
    }

    #[test]
    fn test_lane_event_timestamp() {
        let before = chrono::Utc::now();
        let event = LaneEvent::empty("test.event");
        let after = chrono::Utc::now();

        assert!(event.timestamp >= before);
        assert!(event.timestamp <= after);
    }

    #[test]
    fn test_event_emitter_new() {
        let emitter = EventEmitter::new(100);
        let _receiver = emitter.subscribe();
    }

    #[test]
    fn test_event_emitter_clone() {
        let emitter = EventEmitter::new(100);
        let cloned = emitter.clone();

        emitter.emit(LaneEvent::empty("test.1"));
        cloned.emit(LaneEvent::empty("test.2"));
    }

    #[tokio::test]
    async fn test_event_emitter_subscribe() {
        let emitter = EventEmitter::new(100);
        let mut receiver = emitter.subscribe();

        emitter.emit(LaneEvent::empty("test.event"));

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.key, "test.event");
    }

    #[tokio::test]
    async fn test_event_emitter_multiple_subscribers() {
        let emitter = EventEmitter::new(100);
        let mut receiver1 = emitter.subscribe();
        let mut receiver2 = emitter.subscribe();

        emitter.emit(LaneEvent::with_string("broadcast", "hello"));

        let event1 = receiver1.recv().await.unwrap();
        let event2 = receiver2.recv().await.unwrap();

        assert_eq!(event1.key, "broadcast");
        assert_eq!(event2.key, "broadcast");
    }

    #[tokio::test]
    async fn test_event_emitter_multiple_events() {
        let emitter = EventEmitter::new(100);
        let mut receiver = emitter.subscribe();

        emitter.emit(LaneEvent::empty("event.1"));
        emitter.emit(LaneEvent::empty("event.2"));
        emitter.emit(LaneEvent::empty("event.3"));

        assert_eq!(receiver.recv().await.unwrap().key, "event.1");
        assert_eq!(receiver.recv().await.unwrap().key, "event.2");
        assert_eq!(receiver.recv().await.unwrap().key, "event.3");
    }

    #[tokio::test]
    async fn test_event_stream_filtered() {
        let emitter = EventEmitter::new(100);
        let mut stream = emitter.subscribe_filtered(|e| e.key.starts_with("queue."));

        emitter.emit(LaneEvent::empty("other.event"));
        emitter.emit(LaneEvent::empty("queue.created"));
        emitter.emit(LaneEvent::empty("another.event"));
        emitter.emit(LaneEvent::empty("queue.destroyed"));

        let event1 = stream.recv().await.unwrap();
        assert_eq!(event1.key, "queue.created");

        let event2 = stream.recv().await.unwrap();
        assert_eq!(event2.key, "queue.destroyed");
    }

    #[test]
    fn test_event_payload_serialization() {
        let payload = EventPayload::String("test message".to_string());
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: EventPayload = serde_json::from_str(&json).unwrap();

        if let EventPayload::String(s) = parsed {
            assert_eq!(s, "test message");
        } else {
            panic!("Expected string payload");
        }
    }

    #[test]
    fn test_lane_event_serialization() {
        let event = LaneEvent::with_string("test.event", "hello");
        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains("test.event"));
        assert!(json.contains("hello"));
        assert!(json.contains("timestamp"));

        let parsed: LaneEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.key, "test.event");
    }

    #[tokio::test]
    async fn test_event_stream_implements_stream() {
        use tokio_stream::StreamExt;

        let emitter = EventEmitter::new(100);
        let mut stream = emitter.subscribe_stream();

        emitter.emit(LaneEvent::empty("test.stream.event"));

        let event = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stream.next(),
        )
        .await
        .expect("Timeout waiting for event via Stream::next")
        .expect("Stream ended unexpectedly");

        assert_eq!(event.key, "test.stream.event");
    }

    #[tokio::test]
    async fn test_event_stream_subscribe_stream() {
        use tokio_stream::StreamExt;

        let emitter = EventEmitter::new(100);
        let mut stream = emitter.subscribe_stream();

        emitter.emit(LaneEvent::empty("stream.1"));
        emitter.emit(LaneEvent::empty("stream.2"));
        emitter.emit(LaneEvent::empty("stream.3"));

        for expected in ["stream.1", "stream.2", "stream.3"] {
            let event = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                stream.next(),
            )
            .await
            .expect("Timeout")
            .expect("Stream ended");
            assert_eq!(event.key, expected);
        }
    }

    #[tokio::test]
    async fn test_event_stream_recv_still_works() {
        let emitter = EventEmitter::new(100);
        let mut stream = emitter.subscribe_filtered(|e| e.key.starts_with("ok."));

        emitter.emit(LaneEvent::empty("skip.this"));
        emitter.emit(LaneEvent::empty("ok.recv.event"));

        let event = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            stream.recv(),
        )
        .await
        .expect("Timeout waiting for event via recv()")
        .expect("Stream ended");

        assert_eq!(event.key, "ok.recv.event");
    }

    #[test]
    fn test_event_catalog() {
        assert_eq!(events::QUEUE_LANE_PRESSURE, "queue.lane.pressure");
        assert_eq!(events::QUEUE_LANE_IDLE, "queue.lane.idle");
        assert_eq!(events::QUEUE_COMMAND_SUBMITTED, "queue.command.submitted");
        assert_eq!(events::QUEUE_COMMAND_STARTED, "queue.command.started");
        assert_eq!(events::QUEUE_COMMAND_COMPLETED, "queue.command.completed");
        assert_eq!(events::QUEUE_COMMAND_FAILED, "queue.command.failed");
        assert_eq!(events::QUEUE_COMMAND_TIMEOUT, "queue.command.timeout");
        assert_eq!(events::QUEUE_COMMAND_RETRY, "queue.command.retry");
        assert_eq!(
            events::QUEUE_COMMAND_DEAD_LETTERED,
            "queue.command.dead_lettered"
        );
        assert_eq!(events::QUEUE_SHUTDOWN_STARTED, "queue.shutdown.started");
        assert_eq!(events::QUEUE_SHUTDOWN_COMPLETE, "queue.shutdown.complete");
    }
}
