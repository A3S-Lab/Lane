use pyo3::prelude::*;
use pyo3::types::PyType;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::runtime::Runtime;

use a3s_lane::{EventEmitter, JsonCommand, LaneConfig, QueueManager, QueueManagerBuilder};

use crate::event::PyEventStream;
use crate::types::{json_to_py, PyLaneConfig, PyLaneStatus, PyQueueStats};

/// Priority command queue manager.
#[pyclass]
pub struct PyLane {
    manager: Arc<QueueManager>,
    runtime: Runtime,
}

#[pymethods]
impl PyLane {
    /// Create a queue with the six default lanes (system/control/query/session/skill/prompt).
    #[new]
    fn new() -> PyResult<Self> {
        let runtime = Runtime::new()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        let manager = runtime
            .block_on(async {
                let emitter = EventEmitter::new(100);
                QueueManagerBuilder::new(emitter)
                    .with_default_lanes()
                    .build()
                    .await
            })
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        Ok(Self {
            manager: Arc::new(manager),
            runtime,
        })
    }

    /// Create a queue with custom lanes.
    ///
    /// ```python
    /// lane = Lane.with_lanes([
    ///     LaneConfig("high", priority=0, min_concurrency=1, max_concurrency=4),
    ///     LaneConfig("low",  priority=1, min_concurrency=1, max_concurrency=2),
    /// ])
    /// ```
    #[classmethod]
    fn with_lanes(_cls: &Bound<'_, PyType>, configs: Vec<PyLaneConfig>) -> PyResult<Self> {
        let runtime = Runtime::new()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        let manager = runtime
            .block_on(async {
                let emitter = EventEmitter::new(100);
                let mut builder = QueueManagerBuilder::new(emitter);
                for config in configs {
                    let mut lc =
                        LaneConfig::new(config.min_concurrency, config.max_concurrency);
                    if let Some(secs) = config.timeout_secs {
                        lc = lc.with_timeout(std::time::Duration::from_secs(secs));
                    }
                    if let Some(threshold) = config.pressure_threshold {
                        lc = lc.with_pressure_threshold(threshold);
                    }
                    builder =
                        builder.with_lane(&config.lane_id, lc, config.priority as u8);
                }
                builder.build().await
            })
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        Ok(Self {
            manager: Arc::new(manager),
            runtime,
        })
    }

    /// Start the queue scheduler.
    fn start(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| {
            self.runtime.block_on(async {
                self.manager
                    .start()
                    .await
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
            })
        })
    }

    /// Submit a command to a lane. Blocks until the command completes.
    #[pyo3(signature = (lane_id, command_type, payload=None))]
    fn submit(
        &self,
        py: Python<'_>,
        lane_id: String,
        command_type: String,
        payload: Option<PyObject>,
    ) -> PyResult<PyObject> {
        let json_payload = match payload {
            Some(obj) => {
                let json_mod = PyModule::import_bound(py, "json")?;
                let json_str: String = json_mod.call_method1("dumps", (obj,))?.extract()?;
                serde_json::from_str(&json_str)
                    .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?
            }
            None => serde_json::Value::Null,
        };

        let result = py.allow_threads(|| {
            self.runtime.block_on(async {
                let cmd = Box::new(JsonCommand::new(command_type, json_payload));

                let rx = self
                    .manager
                    .submit(&lane_id, cmd)
                    .await
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

                rx.await
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
            })
        })?;

        Ok(json_to_py(py, &result))
    }

    /// Subscribe to all queue lifecycle events.
    ///
    /// Returns an `EventStream`. Call `stream.recv()` to block until the next event.
    ///
    /// ```python
    /// stream = lane.subscribe()
    /// event = stream.recv(timeout_ms=5000)
    /// if event:
    ///     print(event.key, event.payload)
    /// ```
    fn subscribe(&self) -> PyResult<PyEventStream> {
        let stream = self.manager.subscribe();
        Ok(PyEventStream::new(stream, self.runtime.handle().clone()))
    }

    /// Subscribe to a filtered subset of events by exact key match.
    ///
    /// ```python
    /// failures = lane.subscribe_filtered([
    ///     "queue.command.failed",
    ///     "queue.command.timeout",
    /// ])
    /// ```
    fn subscribe_filtered(&self, keys: Vec<String>) -> PyResult<PyEventStream> {
        let key_set: HashSet<String> = keys.into_iter().collect();
        let stream = self.manager.subscribe_filtered(move |e| key_set.contains(&e.key));
        Ok(PyEventStream::new(stream, self.runtime.handle().clone()))
    }

    /// Get queue statistics.
    fn stats(&self, py: Python<'_>) -> PyResult<PyQueueStats> {
        py.allow_threads(|| {
            self.runtime.block_on(async {
                let stats = self
                    .manager
                    .stats()
                    .await
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

                let lanes: Vec<PyLaneStatus> = stats
                    .lanes
                    .iter()
                    .map(|(id, s)| PyLaneStatus {
                        lane_id: id.clone(),
                        pending: s.pending,
                        active: s.active,
                        min_concurrency: s.min,
                        max_concurrency: s.max,
                    })
                    .collect();

                Ok(PyQueueStats {
                    total_pending: stats.total_pending,
                    total_active: stats.total_active,
                    lanes,
                })
            })
        })
    }

    /// Stop accepting new commands.
    fn shutdown(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| {
            self.runtime.block_on(async {
                self.manager.shutdown().await;
            });
            Ok(())
        })
    }

    /// Wait for all in-flight commands to finish. Call after `shutdown()`.
    ///
    /// ```python
    /// lane.shutdown()
    /// lane.drain(timeout_secs=30.0)
    /// ```
    fn drain(&self, py: Python<'_>, timeout_secs: f64) -> PyResult<()> {
        let duration = std::time::Duration::from_secs_f64(timeout_secs);
        py.allow_threads(|| {
            self.runtime.block_on(async {
                self.manager
                    .drain(duration)
                    .await
                    .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
            })
        })
    }
}
