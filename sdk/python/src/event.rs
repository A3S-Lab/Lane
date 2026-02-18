use pyo3::prelude::*;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::Mutex;

use a3s_lane::{EventPayload, EventStream};

use crate::types::{json_to_py, PyLaneEvent};

/// Async event stream. Call `recv()` to block until the next event arrives.
#[pyclass]
pub struct PyEventStream {
    stream: Arc<Mutex<EventStream>>,
    handle: Handle,
}

impl PyEventStream {
    pub fn new(stream: EventStream, handle: Handle) -> Self {
        Self {
            stream: Arc::new(Mutex::new(stream)),
            handle,
        }
    }
}

#[pymethods]
impl PyEventStream {
    /// Block until the next event arrives, or until `timeout_ms` elapses.
    ///
    /// Returns `None` on timeout or when the queue has shut down.
    #[pyo3(signature = (timeout_ms=None))]
    fn recv(&self, py: Python<'_>, timeout_ms: Option<u64>) -> PyResult<Option<PyLaneEvent>> {
        let stream = self.stream.clone();
        let handle = self.handle.clone();

        let event = py.allow_threads(move || {
            handle.block_on(async move {
                let mut guard = stream.lock().await;
                if let Some(ms) = timeout_ms {
                    tokio::time::timeout(
                        std::time::Duration::from_millis(ms),
                        guard.recv(),
                    )
                    .await
                    .ok()
                    .flatten()
                } else {
                    guard.recv().await
                }
            })
        });

        match event {
            None => Ok(None),
            Some(e) => {
                let payload = match &e.payload {
                    EventPayload::Empty => py.None(),
                    EventPayload::String(s) => s.into_py(py),
                    EventPayload::Map(m) => {
                        let v = serde_json::Value::Object(
                            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        );
                        json_to_py(py, &v)
                    }
                };
                Ok(Some(PyLaneEvent {
                    key: e.key,
                    payload,
                    timestamp: e.timestamp.to_rfc3339(),
                }))
            }
        }
    }
}
