//! N+1 query detection — a static data-flow lint for the most common API-client
//! performance bug: fetching a collection, then issuing **one request per element**.
//!
//! Detecting it robustly takes real provenance tracking, not pattern-matching on syntax.
//! As we walk each body we maintain two taint sets that flow through `let`s, `match`
//! bindings and nested scopes:
//!
//!   * **network-derived** — values that came from a `fetch` (transitively). This is the
//!     "1" in "1 + N": the collection that was itself fetched.
//!   * **loop-tainted** — values derived from an enclosing `for` variable (the element, and
//!     anything computed from it). A `fetch` whose URL/params/body read a loop-tainted value
//!     runs once *per element* — that is the "N".
//!
//! A `fetch` inside a loop that depends on the loop element is the **N+1** pattern; one that
//! depends on nothing in the loop is a **loop-invariant** request (the identical call resent
//! every iteration). Both are reported, with the fix (batch the collection / hoist the call).
//! Severity is *warning*: the program is still valid — the hard SLA is a `budget(...)`, which
//! [`crate::cost`] proves cannot be met once a per-element fetch makes the cost unbounded.

use std::collections::{BTreeSet, HashSet};

use hale_syntax::ast::*;
use hale_syntax::diag::{Diagnostic, Diagnostics};

use crate::lower::free_vars_of;

/// Detect N+1 (and loop-invariant) request patterns across every body in the program.
pub fn detect(program: &Program) -> Diagnostics {
    let mut det = Detector {
        diags: Diagnostics::new(),
        batch_endpoints: endpoints_with_batch(program),
    };

    // The top-level script ("main").
    let main: Vec<Stmt> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Stmt(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    det.walk(&main, &mut Flow::default(), &[]);

    for item in &program.items {
        match item {
            Item::Flow(f) => det.walk(&f.body.stmts, &mut Flow::default(), &[]),
            Item::Test(t) => det.walk(&t.body.stmts, &mut Flow::default(), &[]),
            Item::Server(s) => {
                for r in &s.routes {
                    det.walk(&r.handler.stmts, &mut Flow::default(), &[]);
                }
            }
            _ => {}
        }
    }
    det.diags
}

/// Per-scope data-flow state. Cloned at scope boundaries (loop bodies, match arms) so a
/// binding made inside a branch does not leak to its siblings.
#[derive(Clone, Default)]
struct Flow {
    /// Variables that hold data fetched from the network (transitively).
    network: HashSet<String>,
    /// Variables derived from an enclosing `for` element (the element itself + anything
    /// computed from it). A fetch reading one of these scales with the collection.
    loop_tainted: HashSet<String>,
}

/// One enclosing `for` loop, for diagnostics (the collection it iterates + how it was got).
#[derive(Clone)]
struct LoopFrame {
    var: String,
    collection: String,
    /// `true` if the iterated collection was itself fetched (the classic 1+N).
    collection_fetched: bool,
}

struct Detector {
    diags: Diagnostics,
    /// Endpoints that declare a `batch:` rule — used to tailor the suggested fix.
    batch_endpoints: HashSet<String>,
}

impl Detector {
    /// Walk a statement list, threading the data-flow `flow` and the enclosing-loop stack.
    fn walk(&mut self, stmts: &[Stmt], flow: &mut Flow, loops: &[LoopFrame]) {
        for s in stmts {
            self.walk_stmt(s, flow, loops);
        }
    }

    fn walk_stmt(&mut self, s: &Stmt, flow: &mut Flow, loops: &[LoopFrame]) {
        match s {
            Stmt::Fetch(f) => self.visit_fetch(f, flow, loops),
            Stmt::Let { name, value, .. } => {
                let fv = free_vars_of(value);
                self.assign(flow, &name.node, &fv);
            }
            Stmt::Expr { expr, bind, .. } => {
                // A `match` in statement position can carry fetches/loops in its arms.
                if let Expr::Match(m) = expr {
                    self.visit_match(m, flow, loops);
                }
                if let Some(b) = bind {
                    let fv = free_vars_of(expr);
                    self.assign(flow, &b.name.node, &fv);
                }
            }
            Stmt::Parallel { block, .. } => {
                // A parallel block shares the enclosing scope; bindings flow outward.
                self.walk(&block.stmts, flow, loops);
            }
            Stmt::ForEach {
                var, iter, body, ..
            } => self.visit_for(var, iter, body, flow, loops),
            // Sinks with no binding contribute no new provenance.
            Stmt::Log { .. }
            | Stmt::Assert { .. }
            | Stmt::Return { .. }
            | Stmt::UsingMock { .. } => {}
        }
    }

    fn visit_for(
        &mut self,
        var: &Name,
        iter: &Expr,
        body: &Block,
        flow: &mut Flow,
        loops: &[LoopFrame],
    ) {
        let iter_fv = free_vars_of(iter);
        let collection_fetched = iter_fv.iter().any(|v| flow.network.contains(v));

        // Inside the body the element is loop-tainted, and network-derived iff its
        // collection was. Branch into a child scope so body-locals don't escape.
        let mut child = flow.clone();
        child.loop_tainted.insert(var.node.clone());
        if collection_fetched {
            child.network.insert(var.node.clone());
        } else {
            child.network.remove(&var.node);
        }

        let mut nested = loops.to_vec();
        nested.push(LoopFrame {
            var: var.node.clone(),
            collection: hale_syntax::pretty::expr(iter),
            collection_fetched,
        });
        self.walk(&body.stmts, &mut child, &nested);
    }

    fn visit_match(&mut self, m: &MatchExpr, flow: &Flow, loops: &[LoopFrame]) {
        let scrut_fv = free_vars_of(&m.scrutinee);
        let scrut_net = scrut_fv.iter().any(|v| flow.network.contains(v));
        let scrut_loop = scrut_fv.iter().any(|v| flow.loop_tainted.contains(v));
        for arm in &m.arms {
            let mut child = flow.clone();
            // A pattern binding (`Ok(x)`, `Err(e)`) inherits the scrutinee's provenance.
            for b in pattern_bindings(&arm.pattern) {
                if scrut_net {
                    child.network.insert(b.clone());
                }
                if scrut_loop {
                    child.loop_tainted.insert(b.clone());
                }
            }
            if let ArmBody::Block(block) = &arm.body {
                self.walk(&block.stmts, &mut child, loops);
            }
        }
    }

    fn visit_fetch(&mut self, f: &FetchStmt, flow: &mut Flow, loops: &[LoopFrame]) {
        let inputs = fetch_input_vars(f);
        let depends_on_loop = inputs.iter().any(|v| flow.loop_tainted.contains(v));

        if let Some(inner) = loops.last() {
            if depends_on_loop {
                // Stay silent when the optimizer will auto-fuse this loop (the compiler fixes
                // it, so there is nothing for the user to do). The check is a strict subset of
                // the optimizer's fuse condition, so a suppressed warning always corresponds to
                // an actual fusion — never a silent N+1.
                if !self.auto_fusable(f, &inner.var) {
                    self.report_n_plus_one(f, loops, inner);
                }
            } else {
                self.report_loop_invariant(f, inner);
            }
        }

        // Provenance of the result binding.
        if let Some(b) = &f.bind {
            // A GET result is network data; any fetch result is at least network-derived.
            flow.network.insert(b.name.node.clone());
            // It varies per element only if its inputs did (otherwise it is constant).
            if depends_on_loop {
                flow.loop_tainted.insert(b.name.node.clone());
            } else {
                flow.loop_tainted.remove(&b.name.node);
            }
        }
    }

    /// Whether the optimizer will auto-fuse this per-element loop fetch into one batched call.
    /// Deliberately a *strict subset* of [`crate::optimize`]'s fuse condition: a batch-enabled
    /// GET with a literal collection prefix whose last path segment is exactly the loop element.
    /// Keeping it stricter guarantees that suppressing the warning never hides an unfused N+1.
    fn auto_fusable(&self, f: &FetchStmt, loop_var: &str) -> bool {
        if !self.batch_endpoints.contains(&f.endpoint.node) {
            return false;
        }
        let simple = f.method == "GET"
            && f.params.is_empty()
            && f.pipeline.is_empty()
            && f.idempotency_key.is_none()
            && f.bind.is_some()
            && f.path.segments.len() >= 2;
        if !simple {
            return false;
        }
        // The collection prefix must be all-literal (loop-invariant for sure).
        let n = f.path.segments.len();
        if !f.path.segments[..n - 1]
            .iter()
            .all(|s| matches!(s, PathSeg::Literal(_)))
        {
            return false;
        }
        // The last segment must be exactly the loop element (the strict, common case).
        match f.path.segments.last() {
            Some(PathSeg::Param(key)) => {
                let fv = free_vars_of(key);
                fv.len() == 1 && fv.contains(loop_var)
            }
            _ => false,
        }
    }

    fn report_n_plus_one(&mut self, f: &FetchStmt, loops: &[LoopFrame], inner: &LoopFrame) {
        let path = render_path(&f.path);
        let nesting = if loops.len() > 1 {
            format!(
                " (nested {} loops deep — roughly Nˆ{} requests)",
                loops.len(),
                loops.len()
            )
        } else {
            String::new()
        };
        let head = format!(
            "N+1 query: `{} {}` runs once per element of `{}`{}",
            f.endpoint.node, path, inner.collection, nesting
        );
        let note = if inner.collection_fetched {
            "the classic 1+N: the collection was itself fetched, then one request fires per element — total requests scale with the response size"
        } else {
            "a request fires for every element — the count scales with the collection size, not the source"
        };
        let help = if self.batch_endpoints.contains(&f.endpoint.node) {
            format!(
                "`{}` declares a `batch:` rule — gather the ids across `{}` and fetch them in one batched call instead of one per element",
                f.endpoint.node, inner.collection
            )
        } else {
            format!(
                "fetch the related data in one bulk request (declare `batch: param(\"ids\") key(.id)` on `{}` and request all ids together), or restructure to avoid the per-element call",
                f.endpoint.node
            )
        };
        self.diags.push(
            Diagnostic::warning(f.span, head)
                .with_help(help)
                .with_note(note),
        );
    }

    fn report_loop_invariant(&mut self, f: &FetchStmt, inner: &LoopFrame) {
        // A mutation inside a loop is intentional (it acts on each element's effect), so we
        // only flag *reads* that ignore the element — those are pure waste.
        if f.method != "GET" && f.method != "HEAD" {
            return;
        }
        let path = render_path(&f.path);
        self.diags.push(
            Diagnostic::warning(
                f.span,
                format!(
                    "loop-invariant request: `{} {}` does not depend on `{}`, yet runs on every iteration",
                    f.endpoint.node, path, inner.var
                ),
            )
            .with_help(format!(
                "hoist this fetch above the `for {} in …` loop and run it once",
                inner.var
            ))
            .with_note("dedup only collapses identical calls within one scope; a loop re-issues this every pass"),
        );
    }

    /// Record the provenance of a newly-bound variable from the free vars of its initializer.
    fn assign(&self, flow: &mut Flow, name: &str, from: &BTreeSet<String>) {
        if from.iter().any(|v| flow.network.contains(v)) {
            flow.network.insert(name.to_string());
        } else {
            flow.network.remove(name);
        }
        if from.iter().any(|v| flow.loop_tainted.contains(v)) {
            flow.loop_tainted.insert(name.to_string());
        } else {
            flow.loop_tainted.remove(name);
        }
    }
}

/// The variables that shape a fetch's *request* — its path params, query params, body,
/// idempotency key and pipeline predicates. (The binding is the output, not an input.)
fn fetch_input_vars(f: &FetchStmt) -> BTreeSet<String> {
    let mut v = BTreeSet::new();
    for seg in &f.path.segments {
        if let PathSeg::Param(e) = seg {
            v.extend(free_vars_of(e));
        }
    }
    for (_, e) in &f.params {
        v.extend(free_vars_of(e));
    }
    if let Some(b) = &f.body {
        v.extend(free_vars_of(b));
    }
    if let Some(k) = &f.idempotency_key {
        v.extend(free_vars_of(k));
    }
    for op in &f.pipeline {
        for e in pipeline_exprs(op) {
            v.extend(free_vars_of(e));
        }
    }
    v
}

fn pipeline_exprs(op: &PipelineOp) -> Vec<&Expr> {
    match op {
        PipelineOp::Filter { lambda, .. } | PipelineOp::Map { lambda, .. } => vec![lambda],
        PipelineOp::Sort { by, .. } => vec![by],
        PipelineOp::Limit { count, .. } | PipelineOp::Skip { count, .. } => vec![count],
        PipelineOp::Unique { by: Some(e), .. } | PipelineOp::Sum { by: Some(e), .. } => vec![e],
        _ => vec![],
    }
}

fn pattern_bindings(p: &Pattern) -> Vec<String> {
    let mut out = Vec::new();
    fn go(p: &Pattern, out: &mut Vec<String>) {
        match p {
            Pattern::Binding(n) => out.push(n.node.clone()),
            Pattern::Wildcard(_) => {}
            Pattern::Ctor { args, .. } => {
                for a in args {
                    go(a, out);
                }
            }
        }
    }
    go(p, &mut out);
    out
}

/// Endpoints declaring a `batch:` rule (so the suggested fix can name the mechanism).
fn endpoints_with_batch(program: &Program) -> HashSet<String> {
    let mut s = HashSet::new();
    for item in &program.items {
        if let Item::Endpoint(e) = item {
            if e.settings.iter().any(|st| st.key.node == "batch") {
                s.insert(e.name.node.clone());
            }
        }
    }
    s
}

/// Render a path showing the real parameter expressions (e.g. `/repos/{repo.id}/commits`).
fn render_path(p: &PathPattern) -> String {
    let mut s = String::new();
    for seg in &p.segments {
        s.push('/');
        match seg {
            PathSeg::Literal(l) => s.push_str(l),
            PathSeg::Param(e) => {
                s.push('{');
                s.push_str(&hale_syntax::pretty::expr(e));
                s.push('}');
            }
        }
    }
    if s.is_empty() {
        s.push('/');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warnings(src: &str) -> Vec<String> {
        let (prog, pdiags) = hale_syntax::parse(src);
        assert!(
            !pdiags.has_errors(),
            "parse error:\n{}",
            pdiags.render(src, "t")
        );
        detect(&prog)
            .items()
            .iter()
            .map(|d| d.message.clone())
            .collect()
    }

    #[test]
    fn classic_one_plus_n_over_a_fetched_collection() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /users/gabriel/repos -> repos
            for repo in repos {
                fetch GH /repos/{repo.id}/commits -> commits
                log "{commits.length}"
            }
        "#;
        let w = warnings(src);
        assert!(
            w.iter()
                .any(|m| m.contains("N+1 query") && m.contains("once per element of `repos`")),
            "{w:?}"
        );
    }

    #[test]
    fn n_plus_one_through_an_intermediate_let() {
        // The fetch reads `id`, not the element directly — provenance must be transitive.
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /users -> users
            for u in users {
                let id = u.id
                fetch GH /users/{id}/profile -> p
                log "{p}"
            }
        "#;
        let w = warnings(src);
        assert!(w.iter().any(|m| m.contains("N+1 query")), "{w:?}");
    }

    #[test]
    fn loop_invariant_fetch_is_flagged_separately() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /users -> users
            for u in users {
                fetch GH /config -> cfg
                log "{u.id} {cfg}"
            }
        "#;
        let w = warnings(src);
        assert!(
            w.iter().any(|m| m.contains("loop-invariant request")),
            "{w:?}"
        );
        assert!(
            !w.iter().any(|m| m.contains("N+1 query")),
            "a loop-invariant read is not an N+1: {w:?}"
        );
    }

    #[test]
    fn a_loop_with_no_fetch_is_silent() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /users -> users
            for u in users {
                log "{u.id}"
            }
        "#;
        assert!(warnings(src).is_empty(), "{:?}", warnings(src));
    }

    #[test]
    fn nested_loops_report_quadratic_fanout() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /orgs -> orgs
            for org in orgs {
                fetch GH /orgs/{org.id}/members -> members
                for m in members {
                    fetch GH /users/{m.id} -> profile
                    log "{profile}"
                }
            }
        "#;
        let w = warnings(src);
        assert!(w.iter().any(|m| m.contains("nested 2 loops deep")), "{w:?}");
    }

    #[test]
    fn a_fusable_loop_is_not_warned_about() {
        // The endpoint declares a batch rule and the shape is fusable, so the optimizer will
        // hoist+batch it — the detector stays quiet rather than nagging about a bug the
        // compiler fixes. (The fusion itself is reported by the optimizer, not here.)
        let src = r#"
            endpoint GH { base: "x"  batch: param("ids") key(.id) }
            fetch GH /users -> users
            for u in users {
                fetch GH /users/{u.id} -> detail
                log "{detail}"
            }
        "#;
        let w = warnings(src);
        assert!(
            !w.iter().any(|m| m.contains("N+1 query")),
            "a fusable loop should not be warned about: {w:?}"
        );
    }

    #[test]
    fn batch_rule_but_unfusable_shape_still_warns_with_a_suggestion() {
        // The endpoint has a batch rule but the per-element key is *not* the last path segment
        // (so it cannot be auto-fused) — the detector still warns, pointing at batching.
        let src = r#"
            endpoint GH { base: "x"  batch: param("ids") key(.id) }
            fetch GH /users -> users
            for u in users {
                fetch GH /users/{u.id}/repos -> repos
                log "{repos}"
            }
        "#;
        let w = warnings(src);
        assert!(
            w.iter().any(|m| m.contains("N+1 query")),
            "an unfusable shape must still warn: {w:?}"
        );
    }
}
