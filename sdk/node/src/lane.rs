use std::sync::Arc;

use napi::Result;
use napi_derive::napi;
use tokio::runtime::Runtime;

use a3s_lane::{EventEmitter, JsonCommand, QueueManager, QueueManagerBuilder};

use crate::types::{JsLaneStatus, JsQueueStats};

fn to_napi_error(e: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Priority command queue manager.
///
/// Provides a high-performance, priority-based command queue for async task scheduling.
#[napi]
pub struct JsLane {
    manager: Arc<QueueManager>,
    runtime: Runtime,
}

#[napi]
impl JsLane {
    /// Create a new Lane with default lanes (system, control, query, session, skill, prompt).
    #[napi(constructor)]
    pub fn new() -> Result<Self> {
        let runtime = Runtime::new().map_err(to_napi_error)?;

        let manager = runtime
            .block_on(async {
                let emitter = EventEmitter::new(100);
                QueueManagerBuilder::new(emitter)
                    .with_default_lanes()
                    .build()
                    .await
            })
            .map_err(to_napi_error)?;

        Ok(Self {
            manager: Arc::new(manager),
            runtime,
        })
    }

    /// Start the queue scheduler.
    #[napi]
    pub fn start(&self) -> Result<()> {
        self.runtime
            .block_on(self.manager.start())
            .map_err(to_napi_error)
    }

    /// Submit a command to a lane.
    ///
    /// Returns the command result as a JSON string.
    #[napi]
    pub fn submit(
        &self,
        lane_id: String,
        command_type: String,
        payload: Option<String>,
    ) -> Result<String> {
        let json_payload: serde_json::Value = match payload {
            Some(s) => serde_json::from_str(&s).map_err(to_napi_error)?,
            None => serde_json::Value::Null,
        };

        let result = self.runtime.block_on(async {
            let cmd = Box::new(JsonCommand::new(command_type, json_payload));

            let rx = self
                .manager
                .submit(&lane_id, cmd)
                .await
                .map_err(to_napi_error)?;

            rx.await
                .map_err(to_napi_error)?
                .map_err(to_napi_error)
        })?;

        serde_json::to_string(&result).map_err(to_napi_error)
    }

    /// Get queue statistics.
    #[napi]
    pub fn stats(&self) -> Result<JsQueueStats> {
        let stats = self
            .runtime
            .block_on(self.manager.stats())
            .map_err(to_napi_error)?;

        let lanes: Vec<JsLaneStatus> = stats
            .lanes
            .iter()
            .map(|(id, s)| JsLaneStatus {
                lane_id: id.clone(),
                pending: s.pending as u32,
                active: s.active as u32,
                min_concurrency: s.min as u32,
                max_concurrency: s.max as u32,
            })
            .collect();

        Ok(JsQueueStats {
            total_pending: stats.total_pending as u32,
            total_active: stats.total_active as u32,
            lanes,
        })
    }

    /// Gracefully shut down the queue.
    #[napi]
    pub fn shutdown(&self) -> Result<()> {
        self.runtime.block_on(self.manager.shutdown());
        Ok(())
    }
}
