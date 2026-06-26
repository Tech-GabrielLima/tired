//! `hale-runtime` — the executor and its backends. This is the only crate that pulls
//! in third-party code (tokio + reqwest): the compiler front-end stays dependency-free,
//! and everything network-facing is isolated here.
//!
//! [`Runtime`] owns a compiled program and runs it: `run` for the top-level script,
//! `run_flow` for a named flow, and `run_tests` for the `test` blocks (against mocks).

mod config;
mod contracts;
mod eval;
mod exec;
mod http;
pub mod infer;
mod mock;
mod record;
pub mod schema;
mod server;
pub mod value;

use std::collections::HashMap;
use std::sync::Arc;

use hale_compiler::Compiled;
use hale_syntax::ast::Item;

use config::EndpointConfig;
use http::{HttpEngine, MetricsSnapshot};
use mock::MockEngine;

pub use eval::Env;
pub use record::RecordMode;
pub use value::{RunError, Value};

/// Shared, immutable-after-construction runtime state. Cloned (via `Arc`) into every
/// concurrently-executing fetch task.
pub(crate) struct Shared {
    compiled: Compiled,
    endpoints: HashMap<String, EndpointConfig>,
    mocks: HashMap<String, MockEngine>,
    http: HttpEngine,
    record: RecordMode,
}

/// Fetch a JSON document from a URL (used by `hale inspect`). Independent of any
/// declared endpoint — a plain authenticated-less GET.
pub async fn fetch_json(url: &str) -> Result<serde_json::Value, String> {
    let client = reqwest::Client::builder()
        .user_agent("hale/0.1")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    serde_json::from_str(&text).map_err(|e| format!("invalid JSON: {e}"))
}

pub struct Runtime {
    shared: Arc<Shared>,
}

/// The result of running the `test` blocks.
#[derive(Debug, Default)]
pub struct TestReport {
    pub passed: usize,
    pub failures: Vec<(String, String)>,
}

impl TestReport {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
    pub fn total(&self) -> usize {
        self.passed + self.failures.len()
    }
}

impl Runtime {
    pub fn new(compiled: Compiled) -> Self {
        Self::with_mode(compiled, RecordMode::Off)
    }

    /// Build a runtime with a record/replay mode (see [`RecordMode`]).
    pub fn with_mode(compiled: Compiled, record: RecordMode) -> Self {
        let mut endpoints = HashMap::new();
        let mut mocks = HashMap::new();
        for item in &compiled.program.items {
            match item {
                Item::Endpoint(e) => {
                    endpoints.insert(e.name.node.clone(), EndpointConfig::from_decl(e));
                }
                Item::Mock(m) => {
                    mocks.insert(m.name.node.clone(), MockEngine::from_decl(m));
                }
                _ => {}
            }
        }
        Runtime {
            shared: Arc::new(Shared {
                compiled,
                endpoints,
                mocks,
                http: HttpEngine::new(),
                record,
            }),
        }
    }

    /// Write captured requests to `path` (record mode only). Returns the number written.
    pub fn save_recording(&self, path: &str) -> Result<(), String> {
        match self.shared.record.to_json_string() {
            Some(json) => {
                std::fs::write(path, json).map_err(|e| format!("cannot write `{path}`: {e}"))
            }
            None => Err("not in record mode".to_string()),
        }
    }

    /// Run the top-level script ("main").
    pub async fn run(&self) -> Result<Option<Value>, RunError> {
        let mut env = Env::new();
        let active = std::collections::HashSet::new();
        self.shared
            .run_body(&self.shared.compiled.main, &mut env, &active)
            .await
    }

    /// Invoke a named flow with the given argument values.
    pub async fn run_flow(&self, name: &str, args: Vec<Value>) -> Result<Value, RunError> {
        let active = std::collections::HashSet::new();
        self.shared.run_flow(name, args, &active).await
    }

    /// Run every `test` block against its declared mocks. Each `assert` that fails (or
    /// any runtime error) marks the test failed.
    pub async fn run_tests(&self) -> TestReport {
        let mut report = TestReport::default();
        for test in &self.shared.compiled.tests {
            let active: std::collections::HashSet<String> = test.mocks.iter().cloned().collect();
            let mut env = Env::new();
            match self.shared.run_body(&test.body, &mut env, &active).await {
                Ok(_) => report.passed += 1,
                Err(e) => report.failures.push((test.description.clone(), e.message)),
            }
        }
        report
    }

    /// Serve a `server` block over HTTP (runs until stopped).
    pub async fn serve(&self, name: &str, port: Option<u16>) -> Result<(), RunError> {
        server::serve(self.shared.clone(), name, port).await
    }

    /// Names of the declared servers (so the CLI can default to the only one).
    pub fn server_names(&self) -> Vec<String> {
        self.shared
            .compiled
            .servers
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        self.shared.http.metrics.snapshot()
    }

    pub fn plan(&self) -> String {
        self.shared.compiled.plan()
    }

    pub fn test_count(&self) -> usize {
        self.shared.compiled.tests.len()
    }
}
