//! Python bindings for hale (PyO3, abi3). The Rust compiler + runtime, callable from
//! Python:
//!
//! ```python
//! import hale
//! hale.check('fetch GitGub /u -> x')         # -> ["unknown endpoint `GitGub`", ...]
//! hale.inspect('{"id": 1, "email": "a@b.c"}', "User")   # -> "type User { ... }"
//! hale.run(open("script.hale").read())       # executes the script
//! ```

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// Type-check a program; returns the list of diagnostic messages (empty == OK).
#[pyfunction]
fn check(src: &str) -> Vec<String> {
    hale_compiler::analyze(src)
        .items()
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

/// `True` if the program type-checks with no errors.
#[pyfunction]
fn is_valid(src: &str) -> bool {
    !hale_compiler::analyze(src).has_errors()
}

/// The inferred parallel-execution plan + request cost (`hale explain`).
#[pyfunction]
fn explain(src: &str) -> PyResult<String> {
    let (compiled, diags) = hale_compiler::compile(src, "<python>");
    match compiled {
        Some(c) => Ok(c.plan()),
        None => Err(PyRuntimeError::new_err(diags.render(src, "<python>"))),
    }
}

/// Run a program's top-level script, returning its result (or `None`).
#[pyfunction]
fn run(src: &str) -> PyResult<Option<String>> {
    let (compiled, diags) = hale_compiler::compile(src, "<python>");
    let Some(compiled) = compiled else {
        return Err(PyRuntimeError::new_err(diags.render(src, "<python>")));
    };
    let rt = hale_runtime::Runtime::new(compiled);
    let trt = tokio::runtime::Runtime::new().map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    match trt.block_on(rt.run()) {
        Ok(Some(v)) => Ok(Some(v.display())),
        Ok(None) => Ok(None),
        Err(e) => Err(PyRuntimeError::new_err(e.message)),
    }
}

/// Infer hale `type`/`contract` declarations from a JSON sample.
#[pyfunction]
#[pyo3(signature = (json_text, name = "Root"))]
fn inspect(json_text: &str, name: &str) -> PyResult<String> {
    let j: serde_json::Value = serde_json::from_str(json_text)
        .map_err(|e| PyRuntimeError::new_err(format!("invalid JSON: {e}")))?;
    Ok(hale_runtime::infer::infer_types(&j, name))
}

/// Export a program's `type`/`contract` declarations as a JSON Schema string.
#[pyfunction]
#[pyo3(signature = (src, title = "hale types"))]
fn json_schema(src: &str, title: &str) -> PyResult<Option<String>> {
    let (program, diags) = hale_syntax::parse(src);
    if diags.has_errors() {
        return Err(PyRuntimeError::new_err(diags.render(src, "<python>")));
    }
    Ok(hale_runtime::schema::to_json_schema(&program, title))
}

#[pymodule]
fn hale(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_function(wrap_pyfunction!(check, m)?)?;
    m.add_function(wrap_pyfunction!(is_valid, m)?)?;
    m.add_function(wrap_pyfunction!(explain, m)?)?;
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(inspect, m)?)?;
    m.add_function(wrap_pyfunction!(json_schema, m)?)?;
    Ok(())
}
