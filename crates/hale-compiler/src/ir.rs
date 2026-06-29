//! The intermediate representation hale optimizes and executes.
//!
//! A [`Body`] is a flat list of [`Node`]s plus the **dependency edges** between them.
//! Crucially the IR makes data dependencies explicit, which is what lets the optimizer
//! turn sequentially-written code into a parallel schedule (see [`crate::optimize`]).
//! Expressions are kept as their AST form — only *statements* are lowered to nodes.

use hale_syntax::ast::{Budget, Effect, Expr, PathPattern, Pattern, PipelineOp};
use hale_syntax::span::Span;

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
    /// Boxed: `FetchIr` is far larger than the other node kinds.
    Fetch(Box<FetchIr>),
    Let(Expr),
    Log(Expr),
    Return(Option<Expr>),
    Assert(Expr),
    /// A bare (possibly bound) expression statement, e.g. a flow call.
    Expr(Expr),
    Match(MatchIr),
    /// Extract one element from a batched fetch's result (produced by request fusion).
    Scatter(ScatterIr),
    /// `for v in iter { body }`. The body is its own optimized [`Body`] (sub-schedule);
    /// it runs once per element of `iter`. Boxed — it carries a whole nested body.
    ForEach(Box<ForEachIr>),
}

#[derive(Clone, Debug)]
pub struct FetchIr {
    pub method: String,
    pub endpoint: String,
    pub endpoint_span: Span,
    pub path: PathPattern,
    pub params: Vec<(String, Expr)>,
    pub body: Option<Expr>,
    /// `idempotent(key: …)` was declared — this write is safe to auto-retry; the key
    /// expression is sent as an `Idempotency-Key` header.
    pub idempotency_key: Option<Expr>,
    /// The endpoint's declared per-hop `latency:` (ms), copied in during lowering so the
    /// cost analysis can bound critical-path latency. `None` ⇒ undeclared.
    pub latency_ms: Option<u64>,
    /// Request-fusion state. `Some` with empty `ids` ⇒ a *candidate* (its endpoint declared
    /// a `batch:` rule); the optimizer fuses a group of candidates into one **batched** fetch
    /// whose `ids` are populated (and whose path is the collection prefix).
    pub batch: Option<BatchSpec>,
    pub pipeline: Vec<PipelineOp>,
    /// `true` if the binding was annotated `Result<...>` (opts into Ok/Err wrapping).
    pub as_result: bool,
    /// The binding's declared type, if any — used for runtime contract validation.
    pub contract_ty: Option<hale_syntax::ast::TypeExpr>,
}

/// Request-fusion specification (from an endpoint's `batch:` rule + the fused values).
#[derive(Clone, Debug)]
pub struct BatchSpec {
    /// Query parameter that carries the comma-joined ids (e.g. `ids`).
    pub query_param: String,
    /// Response-element field to match each id against when scattering (e.g. `id`).
    pub key_field: String,
    /// The individual ids being fused (static fusion). Empty ⇒ either an unfused candidate
    /// (treated as a plain GET) or a *mapped* batch — see `mapped`.
    pub ids: Vec<Expr>,
    /// **Loop fusion.** Instead of a fixed `ids` list, the ids are produced at runtime by
    /// mapping a key expression over a collection (`coll | map(var => key)`). This is what
    /// lets a `for` loop's per-element GET be hoisted into a single batched call.
    pub mapped: Option<MappedIds>,
}

/// The runtime source of a *mapped* batch's ids: evaluate `key` for each element of `coll`
/// (with `var` bound to the element). Produced by loop fusion (see [`crate::optimize`]).
#[derive(Clone, Debug)]
pub struct MappedIds {
    pub coll: Expr,
    pub var: String,
    pub key: Expr,
}

/// A node produced by request fusion: pull one element out of a batched fetch's array
/// result by matching `key_field == value`, and bind it. Pure (no I/O).
#[derive(Clone, Debug)]
pub struct ScatterIr {
    /// Binding of the batched fetch's array result.
    pub batch: String,
    pub key_field: String,
    /// The id this binding corresponds to (the original fetch's varying path value).
    pub value: Expr,
}

#[derive(Clone, Debug)]
pub struct MatchIr {
    pub scrutinee: Expr,
    pub arms: Vec<ArmIr>,
    pub span: Span,
}

/// A compiled `for v in iter { body }` loop. The body is optimized independently, so
/// independent fetches *within one iteration* still parallelize; iterations are sequential.
#[derive(Clone, Debug)]
pub struct ForEachIr {
    /// The element binding, in scope inside `body`.
    pub var: String,
    /// The collection expression (evaluated once).
    pub iter: Expr,
    pub body: Body,
    /// `true` if the body can `return` (directly or inside a nested match/loop) — lets the
    /// executor propagate an early return out of the loop instead of treating the body's
    /// trailing value as one.
    pub returns: bool,
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
    /// Declared SLA, enforced against the cost analysis (see [`crate::cost`]).
    pub budget: Option<Budget>,
    pub body: Body,
}

/// A compiled `test` block.
#[derive(Clone, Debug)]
pub struct Test {
    pub description: String,
    pub mocks: Vec<String>,
    pub body: Body,
}

/// A compiled `server` declaration.
#[derive(Clone, Debug)]
pub struct Server {
    pub name: String,
    pub base: String,
    pub port: Option<u16>,
    pub routes: Vec<Route>,
}

/// One route of a server. Its handler `body` is optimized like any other body, so a
/// fan-out aggregation in the handler is parallelized and deduplicated automatically.
#[derive(Clone, Debug)]
pub struct Route {
    pub method: String,
    pub path: PathPattern,
    pub param_names: Vec<String>,
    /// Declared SLA, enforced against the cost analysis (see [`crate::cost`]).
    pub budget: Option<Budget>,
    pub body: Body,
}

impl NodeKind {
    pub fn label(&self) -> String {
        match self {
            NodeKind::Fetch(f) => {
                let m = if f.method == "GET" {
                    String::new()
                } else {
                    format!("{} ", f.method)
                };
                let idem = if f.idempotency_key.is_some() {
                    "  [idempotent]"
                } else {
                    ""
                };
                match &f.batch {
                    Some(b) if !b.ids.is_empty() => format!(
                        "fetch {} {}?{}=… [batched ×{}]{idem}",
                        f.endpoint,
                        render_path(&f.path),
                        b.query_param,
                        b.ids.len()
                    ),
                    Some(b) if b.mapped.is_some() => format!(
                        "fetch {} {}?{}=… [batched ×N, fused from loop]{idem}",
                        f.endpoint,
                        render_path(&f.path),
                        b.query_param,
                    ),
                    _ => format!("fetch {m}{} {}{idem}", f.endpoint, render_path(&f.path)),
                }
            }
            NodeKind::Let(_) => "let".into(),
            NodeKind::Log(_) => "log".into(),
            NodeKind::Return(_) => "return".into(),
            NodeKind::Assert(_) => "assert".into(),
            NodeKind::Expr(_) => "expr".into(),
            NodeKind::Match(_) => "match".into(),
            NodeKind::Scatter(s) => format!("scatter {}.{} (from batch)", s.batch, s.key_field),
            NodeKind::ForEach(fe) => format!("for {} in … (per-element loop)", fe.var),
        }
    }

    pub fn is_fetch(&self) -> bool {
        matches!(self, NodeKind::Fetch(_))
    }
}

pub fn render_path(p: &PathPattern) -> String {
    use hale_syntax::ast::PathSeg;
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
