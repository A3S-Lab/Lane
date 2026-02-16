use pyo3::prelude::*;
use std::sync::Arc;
use tokio::runtime::Runtime;

use a3s_lane::{EventEmitter, JsonCommand, QueueManager, QueueManagerBuilder};

use crate::types::{json_to_py, PyLaneStatus, PyQueueStats};

/// Priority command queue manager.
#[pyclass]
pub struct PyLane {
    manager: Arc<QueueManager>,
    runtime: Runtime,
}

#[pymethods]
impl PyLane {
    /// Create a new Lane with default lanes.
    #[new]
    fn new() -> PyResult<Self> {
        let runtime = Runtime::new()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;

        let manager = runtime.block_on(async {
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

    /// Submit a command to a lane.
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

    /// Gracefully shut down the queue.
    fn shutdown(&self, py: Python<'_>) -> PyResult<()> {
        py.allow_threads(|| {
            self.runtime.block_on(async {
                self.manager.shutdown().await;
            });
            Ok(())
        })
    }
}
