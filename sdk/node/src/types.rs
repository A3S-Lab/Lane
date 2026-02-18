use napi_derive::napi;

/// Lane configuration for custom queue setup.
#[napi(object)]
#[derive(Clone)]
pub struct JsLaneConfig {
    pub lane_id: String,
    pub priority: u32,
    pub min_concurrency: u32,
    pub max_concurrency: u32,
    pub timeout_secs: Option<u32>,
    pub pressure_threshold: Option<u32>,
}

/// A queue lifecycle event.
#[napi(object)]
#[derive(Clone)]
pub struct JsLaneEvent {
    pub key: String,
    /// JSON-encoded payload. Use `JSON.parse(event.payload)` to get the value.
    /// `"null"` for empty events.
    pub payload: String,
    /// RFC3339 timestamp.
    pub timestamp: String,
}

/// Lane status snapshot.
#[napi(object)]
#[derive(Clone)]
pub struct JsLaneStatus {
    pub lane_id: String,
    pub pending: u32,
    pub active: u32,
    pub min_concurrency: u32,
    pub max_concurrency: u32,
}

/// Queue statistics.
#[napi(object)]
#[derive(Clone)]
pub struct JsQueueStats {
    pub total_pending: u32,
    pub total_active: u32,
    pub lanes: Vec<JsLaneStatus>,
}
