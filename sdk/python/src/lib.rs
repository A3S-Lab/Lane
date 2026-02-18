use pyo3::prelude::*;

mod event;
mod lane;
mod types;

use event::PyEventStream;
use lane::PyLane;
use types::{PyLaneConfig, PyLaneEvent, PyLaneStatus, PyQueueStats};

/// Native Python bindings for a3s-lane priority command queue.
#[pymodule]
fn _a3s_lane(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyLane>()?;
    m.add_class::<PyLaneConfig>()?;
    m.add_class::<PyLaneEvent>()?;
    m.add_class::<PyEventStream>()?;
    m.add_class::<PyLaneStatus>()?;
    m.add_class::<PyQueueStats>()?;
    Ok(())
}
