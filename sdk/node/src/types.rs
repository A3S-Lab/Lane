use napi_derive::napi;

/// Lane configuration.
#[napi(object)]
#[derive(Clone)]
pub struct JsLaneConfig {
    pub min_concurrency: u32,
    pub max_concurrency: u32,
    pub timeout_secs: Option<u32>,
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
