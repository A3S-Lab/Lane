use std::collections::HashSet;
use std::sync::Arc;

use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Result;
use napi_derive::napi;
use tokio::runtime::Runtime;

use a3s_lane::{EventEmitter, EventPayload, JsonCommand, LaneConfig, QueueManager, QueueManagerBuilder};

use crate::types::{JsLaneConfig, JsLaneEvent, JsLaneStatus, JsQueueStats};

fn to_napi_error(e: impl std::fmt::Display) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

fn payload_to_json(payload: &EventPayload) -> String {
    match payload {
        EventPayload::Empty => "null".to_string(),
        EventPayload::String(s) => serde_json::to_string(s).unwrap_or_default(),
        EventPayload::Map(m) => serde_json::to_string(m).unwrap_or_default(),
    }
}

fn build_lane_config(config: &JsLaneConfig) -> LaneConfig {
    let mut lc = LaneConfig::new(config.min_concurrency as usize, config.max_concurrency as usize);
    if let Some(secs) = config.timeout_secs {
        lc = lc.with_timeout(std::time::Duration::from_secs(secs as u64));
    }
    if let Some(threshold) = config.pressure_threshold {
        lc = lc.with_pressure_threshold(threshold as usize);
    }
    lc
}

/// Priority command queue manager.
#[napi]
pub struct JsLane {
    manager: Arc<QueueManager>,
    runtime: Runtime,
}

#[napi]
impl JsLane {
    /// Create a queue with the six default lanes (system/control/query/session/skill/prompt).
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

    /// Create a queue with custom lanes.
    ///
    /// ```js
    /// const lane = Lane.withLanes([
    ///   { laneId: "high", priority: 0, minConcurrency: 1, maxConcurrency: 4 },
    ///   { laneId: "low",  priority: 1, minConcurrency: 1, maxConcurrency: 2 },
    /// ]);
    /// ```
    #[napi(factory)]
    pub fn with_lanes(configs: Vec<JsLaneConfig>) -> Result<Self> {
        let runtime = Runtime::new().map_err(to_napi_error)?;

        let manager = runtime
            .block_on(async {
                let emitter = EventEmitter::new(100);
                let mut builder = QueueManagerBuilder::new(emitter);
                for config in &configs {
                    let lc = build_lane_config(config);
                    builder = builder.with_lane(&config.lane_id, lc, config.priority as u8);
                }
                builder.build().await
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

    /// Submit a command to a lane. Blocks until the command completes.
    ///
    /// Returns the result as a JSON string.
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

    /// Subscribe to all queue lifecycle events.
    ///
    /// The callback receives `(err, event)`. Runs until the queue shuts down.
    ///
    /// ```js
    /// lane.subscribe((err, event) => {
    ///   if (err) throw err;
    ///   console.log(event.key, JSON.parse(event.payload));
    /// });
    /// ```
    #[napi(
        ts_args_type = "callback: (err: null | Error, event: LaneEvent) => void",
        ts_return_type = "void"
    )]
    pub fn subscribe(
        &self,
        callback: ThreadsafeFunction<JsLaneEvent, ErrorStrategy::CalleeHandled>,
    ) -> Result<()> {
        let handle = self.runtime.handle().clone();
        let mut stream = self.manager.subscribe();
        handle.spawn(async move {
            while let Some(event) = stream.recv().await {
                let js_event = JsLaneEvent {
                    key: event.key.clone(),
                    payload: payload_to_json(&event.payload),
                    timestamp: event.timestamp.to_rfc3339(),
                };
                callback.call(Ok(js_event), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }

    /// Subscribe to a filtered subset of events by exact key match.
    ///
    /// ```js
    /// lane.subscribeFiltered(
    ///   ["queue.command.failed", "queue.command.timeout"],
    ///   (err, event) => { console.log("failure:", event.key); }
    /// );
    /// ```
    #[napi(
        ts_args_type = "keys: string[], callback: (err: null | Error, event: LaneEvent) => void",
        ts_return_type = "void"
    )]
    pub fn subscribe_filtered(
        &self,
        keys: Vec<String>,
        callback: ThreadsafeFunction<JsLaneEvent, ErrorStrategy::CalleeHandled>,
    ) -> Result<()> {
        let handle = self.runtime.handle().clone();
        let key_set: HashSet<String> = keys.into_iter().collect();
        let mut stream = self.manager.subscribe_filtered(move |e| key_set.contains(&e.key));
        handle.spawn(async move {
            while let Some(event) = stream.recv().await {
                let js_event = JsLaneEvent {
                    key: event.key.clone(),
                    payload: payload_to_json(&event.payload),
                    timestamp: event.timestamp.to_rfc3339(),
                };
                callback.call(Ok(js_event), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
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

    /// Stop accepting new commands.
    #[napi]
    pub fn shutdown(&self) -> Result<()> {
        self.runtime.block_on(self.manager.shutdown());
        Ok(())
    }

    /// Wait for all in-flight commands to finish. Call after `shutdown()`.
    ///
    /// `timeoutMs` is the maximum wait in milliseconds.
    #[napi]
    pub fn drain(&self, timeout_ms: u32) -> Result<()> {
        let duration = std::time::Duration::from_millis(timeout_ms as u64);
        self.runtime
            .block_on(self.manager.drain(duration))
            .map_err(to_napi_error)
    }
}
