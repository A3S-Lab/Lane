use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use serde_json::Value;

/// Lane configuration.
#[pyclass]
#[derive(Clone)]
pub struct PyLaneConfig {
    #[pyo3(get, set)]
    pub min_concurrency: usize,
    #[pyo3(get, set)]
    pub max_concurrency: usize,
    #[pyo3(get, set)]
    pub timeout_secs: Option<u64>,
}

#[pymethods]
impl PyLaneConfig {
    #[new]
    #[pyo3(signature = (min_concurrency=1, max_concurrency=10, timeout_secs=None))]
    fn new(min_concurrency: usize, max_concurrency: usize, timeout_secs: Option<u64>) -> Self {
        Self {
            min_concurrency,
            max_concurrency,
            timeout_secs,
        }
    }
}

/// Lane status snapshot.
#[pyclass]
#[derive(Clone)]
pub struct PyLaneStatus {
    #[pyo3(get)]
    pub lane_id: String,
    #[pyo3(get)]
    pub pending: usize,
    #[pyo3(get)]
    pub active: usize,
    #[pyo3(get)]
    pub min_concurrency: usize,
    #[pyo3(get)]
    pub max_concurrency: usize,
}

#[pymethods]
impl PyLaneStatus {
    fn __repr__(&self) -> String {
        format!(
            "LaneStatus(lane_id='{}', pending={}, active={}, min={}, max={})",
            self.lane_id, self.pending, self.active, self.min_concurrency, self.max_concurrency
        )
    }
}

/// Queue statistics.
#[pyclass]
#[derive(Clone)]
pub struct PyQueueStats {
    #[pyo3(get)]
    pub total_pending: usize,
    #[pyo3(get)]
    pub total_active: usize,
    #[pyo3(get)]
    pub lanes: Vec<PyLaneStatus>,
}

#[pymethods]
impl PyQueueStats {
    fn __repr__(&self) -> String {
        format!(
            "QueueStats(pending={}, active={}, lanes={})",
            self.total_pending,
            self.total_active,
            self.lanes.len()
        )
    }
}

/// Convert a serde_json::Value to a Python object.
pub fn json_to_py(py: Python<'_>, value: &Value) -> PyObject {
    match value {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_py(py),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py(py)
            } else if let Some(f) = n.as_f64() {
                f.into_py(py)
            } else {
                py.None()
            }
        }
        Value::String(s) => s.into_py(py),
        Value::Array(arr) => {
            let items: Vec<PyObject> = arr.iter().map(|v| json_to_py(py, v)).collect();
            PyList::new_bound(py, items).into_py(py)
        }
        Value::Object(map) => {
            let dict = PyDict::new_bound(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)).unwrap();
            }
            dict.into_py(py)
        }
    }
}
