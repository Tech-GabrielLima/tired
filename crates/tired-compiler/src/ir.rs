//! The intermediate representation TIRED optimizes and executes.
//!
//! A [`Body`] is a flat list of [`Node`]s plus the **dependency edges** between them.
//! Crucially the IR makes data dependencies explicit, which is what lets the optimizer
//! turn sequentially-written code into a parallel schedule (see [`crate::optimize`]).
//! Expressions are kept as their AST form — only *statements* are lowered to nodes.

use tired_syntax::ast::{Effect, Expr, PathPattern, Pattern, PipelineOp};
use tired_syntax::span::Span;

pub type NodeId = usize;

/// A schedulable sequence of operations (a flow body, the top-level script, a test, or
/// a `match` arm). Carries the optimizer's results: liveness flags on nodes and the
/// topological `waves` used by the executor for concurrency.
#[derive(Clone, Debug, Default)]
pub struct Body {
    pub nodes: Vec<Node>,
    /// Live nodes grouped into dependency levels. All nodes in one wave are mutually
    /// independent and are executed concurrently. Filled in by the optimizer.
    pub waves: Vec<Vec<NodeId>>,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// The variable this node binds, if any.
    pub binding: Option<String>,
    /// Free variables read by this node (used to compute `deps`).
    pub reads: Vec<String>,
    /// Ids of earlier nodes in this body that this node depends on.
    pub deps: Vec<NodeId>,
    /// Effects (log/assert/return/match/flow-calls) are kept in their written order and
    /// are always live; pure computations (fetch/let) are scheduled purely by data.
    pub effect: bool,
    /// Set by dead-request elimination. Dead nodes are never executed.
    pub live: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum NodeKind {
    Fetch(FetchIr),
    Let(Expr),
    Log(Expr),
    Return(Option<Expr>),
    Assert(Expr),
    /// A bare (possibly bound) expression statement, e.g. a flow call.
    Expr(Expr),
    Match(MatchIr),
}

#[derive(Clone, Debug)]
pub struct FetchIr {
    pub endpoint: String,
    pub endpoint_span: Span,
    pub path: PathPattern,
    pub params: Vec<(String, Expr)>,
    pub pipeline: Vec<PipelineOp>,
    /// `true` if the binding was annotated `Result<...>` (opts into Ok/Err wrapping).
    pub as_result: bool,
    /// The binding's declared type, if any — used for runtime contract validation.
    pub contract_ty: Option<tired_syntax::ast::TypeExpr>,
}

#[derive(Clone, Debug)]
pub struct MatchIr {
    pub scrutinee: Expr,
    pub arms: Vec<ArmIr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ArmIr {
    pub pattern: Pattern,
    pub body: ArmBodyIr,
}

#[derive(Clone, Debug)]
pub enum ArmBodyIr {
    Value(Expr),
    Body(Body),
    Retry { effects: Vec<Effect> },
}

/// A compiled `flow` declaration.
#[derive(Clone, Debug)]
pub struct Flow {
    pub name: String,
    pub params: Vec<String>,
    pub body: Body,
}

/// A compiled `test` block.
#[derive(Clone, Debug)]
pub struct Test {
    pub description: String,
    pub mocks: Vec<String>,
    pub body: Body,
}

impl NodeKind {
    pub fn label(&self) -> String {
        match self {
            NodeKind::Fetch(f) => format!("fetch {} {}", f.endpoint, render_path(&f.path)),
            NodeKind::Let(_) => "let".into(),
            NodeKind::Log(_) => "log".into(),
            NodeKind::Return(_) => "return".into(),
            NodeKind::Assert(_) => "assert".into(),
            NodeKind::Expr(_) => "expr".into(),
            NodeKind::Match(_) => "match".into(),
        }
    }

    pub fn is_fetch(&self) -> bool {
        matches!(self, NodeKind::Fetch(_))
    }
}

pub fn render_path(p: &PathPattern) -> String {
    use tired_syntax::ast::PathSeg;
    let mut s = String::new();
    for seg in &p.segments {
        s.push('/');
        match seg {
            PathSeg::Literal(l) => s.push_str(l),
            PathSeg::Param(_) => s.push_str("{..}"),
        }
    }
    if s.is_empty() {
        s.push('/');
    }
    s
}
