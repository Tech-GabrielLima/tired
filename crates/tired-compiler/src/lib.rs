//! `tired-compiler` — semantic analysis, the type system, the IR, and the optimizer.
//! Like `tired-syntax`, this crate is **dependency-free**.
//!
//! The end-to-end entry point is [`compile`]: it parses, type-checks, lowers to IR and
//! optimizes, returning a [`Compiled`] program ready for the runtime to execute (or
//! `None` if there were errors).

pub mod check;
pub mod cost;
pub mod ir;
pub mod lower;
pub mod optimize;
pub mod types;

pub use check::{check, Analysis};
pub use types::{ErrDomain, Type, TypeTable};

use tired_syntax::ast::Program;
use tired_syntax::diag::Diagnostics;

/// A fully compiled program: the original AST (the runtime reads endpoint/mock/type
/// declarations from it) plus the optimized IR bodies.
pub struct Compiled {
    pub program: Program,
    pub analysis: Analysis,
    pub main: ir::Body,
    pub flows: Vec<ir::Flow>,
    pub tests: Vec<ir::Test>,
    pub servers: Vec<ir::Server>,
}

impl Compiled {
    pub fn flow(&self, name: &str) -> Option<&ir::Flow> {
        self.flows.iter().find(|f| f.name == name)
    }

    pub fn server(&self, name: &str) -> Option<&ir::Server> {
        self.servers.iter().find(|s| s.name == name)
    }

    /// The human-readable parallel execution plan (`tired explain`).
    pub fn plan(&self) -> String {
        optimize::render_plan(&self.main, &self.flows, &self.tests, &self.servers)
    }
}

/// Parse → check → lower → optimize. Returns the compiled program (when there are no
/// hard errors) together with all diagnostics (which may include warnings).
pub fn compile(src: &str, path: &str) -> (Option<Compiled>, Diagnostics) {
    let (program, mut diags) = tired_syntax::parse(src);
    let _ = path;

    let (analysis, check_diags) = check(&program);
    diags.extend(check_diags);

    let (mut main, mut flows, mut tests, mut servers) = lower::lower_program(&program);
    let opt_diags = optimize::optimize(&mut main, &mut flows, &mut tests, &mut servers);
    diags.extend(opt_diags);

    if diags.has_errors() {
        return (None, diags);
    }
    (
        Some(Compiled {
            program,
            analysis,
            main,
            flows,
            tests,
            servers,
        }),
        diags,
    )
}

/// Type-check only (used by `tired check`). Returns all diagnostics.
pub fn analyze(src: &str) -> Diagnostics {
    let (program, mut diags) = tired_syntax::parse(src);
    let (_an, check_diags) = check(&program);
    diags.extend(check_diags);
    let (mut main, mut flows, mut tests, mut servers) = lower::lower_program(&program);
    let opt_diags = optimize::optimize(&mut main, &mut flows, &mut tests, &mut servers);
    diags.extend(opt_diags);
    diags
}
