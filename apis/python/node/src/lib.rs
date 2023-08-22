#![allow(clippy::borrow_deref_ref)] // clippy warns about code generated by #[pymethods]

use arrow::datatypes::DataType;
use dora_node_api::merged::MergedEvent;
use dora_node_api::{merged::MergeExternal, DoraNode, EventStream};
use dora_operator_api_python::{
    process_python_output, process_python_type, pydict_to_metadata, PyEvent,
};
use eyre::{Context, ContextCompat};
use futures::{Stream, StreamExt};
use pyo3::prelude::*;
use pyo3::types::PyDict;

/// The custom node API lets you integrate `dora` into your application.
/// It allows you to retrieve input and send output in any fashion you want.
///
/// Use with:
///
/// ```python
/// from dora import Node
///
/// node = Node()
/// ```
///
#[pyclass]
pub struct Node {
    events: Events,
    node: DoraNode,
}

#[pymethods]
impl Node {
    #[new]
    pub fn new() -> eyre::Result<Self> {
        let (node, events) = DoraNode::init_from_env()?;

        Ok(Node {
            events: Events::Dora(events),
            node,
        })
    }

    /// `.next()` gives you the next input that the node has received.
    /// It blocks until the next event becomes available.
    /// It will return `None` when all senders has been dropped.
    ///
    /// ```python
    /// event = node.next()
    /// ```
    ///
    /// You can also iterate over the event stream with a loop
    ///
    /// ```python
    /// for event in node:
    ///    match event["type"]:
    ///        case "INPUT":
    ///            match event["id"]:
    ///                 case "image":
    /// ```
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self, py: Python) -> PyResult<Option<PyEvent>> {
        self.__next__(py)
    }

    pub fn __next__(&mut self, py: Python) -> PyResult<Option<PyEvent>> {
        let event = py.allow_threads(|| self.events.recv());
        Ok(event)
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// `send_output` send data from the node.
    ///
    /// ```python
    /// Args:
    ///    output_id: str,
    ///    data: Bytes|Arrow,
    ///    metadata: Option[Dict],
    /// ```
    ///
    /// ```python
    /// node.send_output("string", b"string", {"open_telemetry_context": "7632e76"})
    /// ```
    ///
    pub fn send_output(
        &mut self,
        output_id: String,
        data: PyObject,
        metadata: Option<&PyDict>,
        py: Python,
    ) -> eyre::Result<()> {
        let data_type = process_python_type(&data, py).context("could not get type")?;
        process_python_output(&data, py, |data| {
            self.send_output_slice(output_id, data.len(), data_type, data, metadata)
        })
    }

    /// Returns the full dataflow descriptor that this node is part of.
    ///
    /// This method returns the parsed dataflow YAML file.
    pub fn dataflow_descriptor(&self, py: Python) -> pythonize::Result<PyObject> {
        pythonize::pythonize(py, self.node.dataflow_descriptor())
    }

    pub fn merge_external_events(
        &mut self,
        external_events: &mut ExternalEventStream,
    ) -> eyre::Result<()> {
        // take out the event stream and temporarily replace it with a dummy
        let events = std::mem::replace(
            &mut self.events,
            Events::Merged(Box::new(futures::stream::empty())),
        );
        // update self.events with the merged stream
        self.events = Events::Merged(events.merge_external(Box::pin(
            external_events.0.take().context("stream already taken")?,
        )));

        Ok(())
    }
}

#[pyclass]
pub struct ExternalEventStream(pub Option<Box<dyn Stream<Item = PyObject> + Unpin + Send>>);

impl<S> From<S> for ExternalEventStream
where
    S: Stream<Item = PyObject> + Unpin + Send + 'static,
{
    fn from(value: S) -> Self {
        Self(Some(Box::new(value)))
    }
}

enum Events {
    Dora(EventStream),
    Merged(Box<dyn Stream<Item = MergedEvent<PyObject>> + Unpin + Send>),
}

impl Events {
    fn recv(&mut self) -> Option<PyEvent> {
        match self {
            Events::Dora(events) => events.recv().map(PyEvent::from),
            Events::Merged(events) => futures::executor::block_on(events.next()).map(PyEvent::from),
        }
    }
}

impl<'a> MergeExternal<'a, PyObject> for Events {
    type Item = MergedEvent<PyObject>;

    fn merge_external(
        self,
        external_events: impl Stream<Item = PyObject> + Send + Unpin + 'a,
    ) -> Box<dyn Stream<Item = Self::Item> + Send + Unpin + 'a> {
        match self {
            Events::Dora(events) => events.merge_external(external_events),
            Events::Merged(events) => {
                let merged = events.merge_external(external_events);
                Box::new(merged.map(|event| match event {
                    MergedEvent::Dora(e) => MergedEvent::Dora(e),
                    MergedEvent::External(e) => MergedEvent::External(e.flatten()),
                }))
            }
        }
    }
}

impl Node {
    fn send_output_slice(
        &mut self,
        output_id: String,
        len: usize,
        data_type: DataType,
        data: &[u8],
        metadata: Option<&PyDict>,
    ) -> eyre::Result<()> {
        let parameters = pydict_to_metadata(metadata)?;
        self.node
            .send_typed_output(output_id.into(), data_type, parameters, len, |out| {
                out.copy_from_slice(data);
            })
            .wrap_err("failed to send output")
    }

    pub fn id(&self) -> String {
        self.node.id().to_string()
    }
}

/// Start a runtime for Operators
#[pyfunction]
pub fn start_runtime() -> eyre::Result<()> {
    dora_runtime::main().wrap_err("Dora Runtime raised an error.")
}

#[pymodule]
fn dora(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(start_runtime, m)?)?;
    m.add_class::<Node>().unwrap();
    Ok(())
}
