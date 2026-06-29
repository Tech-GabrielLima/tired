//! Lowering: AST → IR. The interesting work is **dependency analysis** — for each
//! node we compute the set of variables it reads, then wire an edge from a node to the
//! most recent earlier node that wrote each of those variables. Effect nodes (log,
//! return, …) additionally chain to the previous effect so observable order is kept.

use std::collections::BTreeSet;

use hale_syntax::ast::*;
use hale_syntax::span::Span;

use crate::ir::*;

/// Lower a whole program into the top-level body, the flows, the tests and the servers.
pub fn lower_program(program: &Program) -> (Body, Vec<Flow>, Vec<Test>, Vec<Server>) {
    let main_stmts: Vec<Stmt> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Stmt(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let mut main = lower_stmts(&main_stmts);

    let mut flows = Vec::new();
    let mut tests = Vec::new();
    let mut servers = Vec::new();
    for item in &program.items {
        match item {
            Item::Flow(f) => flows.push(Flow {
                name: f.name.node.clone(),
                params: f.params.iter().map(|p| p.name.node.clone()).collect(),
                budget: f.budget.clone(),
                body: lower_stmts(&f.body.stmts),
            }),
            Item::Test(t) => tests.push(Test {
                description: t.description.clone(),
                mocks: collect_mocks(&t.body.stmts),
                body: lower_stmts(&t.body.stmts),
            }),
            Item::Server(s) => servers.push(lower_server(s)),
            _ => {}
        }
    }

    // Copy each endpoint's declared `latency:` and `batch:` rule onto its fetch nodes, so the
    // optimizer / cost analysis can use them without re-reading the AST.
    let latency = endpoint_latencies(program);
    let batch = endpoint_batch_rules(program);
    if !latency.is_empty() || !batch.is_empty() {
        let go = |b: &mut Body| {
            annotate_latency(b, &latency);
            annotate_batch(b, &batch);
        };
        go(&mut main);
        for f in &mut flows {
            go(&mut f.body);
        }
        for t in &mut tests {
            go(&mut t.body);
        }
        for s in &mut servers {
            for r in &mut s.routes {
                go(&mut r.body);
            }
        }
    }
    (main, flows, tests, servers)
}

/// Read each endpoint's `batch: param("ids") key(.id)` rule: (query param, key field).
fn endpoint_batch_rules(program: &Program) -> std::collections::HashMap<String, (String, String)> {
    let mut map = std::collections::HashMap::new();
    for item in &program.items {
        if let Item::Endpoint(e) = item {
            if let Some(s) = e.settings.iter().find(|s| s.key.node == "batch") {
                let mut param = None;
                let mut key = None;
                for v in &s.values {
                    if let Expr::Call { callee, args, .. } = v {
                        if let Expr::Ident(n) = callee.as_ref() {
                            match (n.node.as_str(), args.first()) {
                                ("param", Some(a)) => param = str_atom(a),
                                ("key", Some(a)) => key = str_atom(a),
                                _ => {}
                            }
                        }
                    }
                }
                if let (Some(p), Some(k)) = (param, key) {
                    map.insert(e.name.node.clone(), (p, k));
                }
            }
        }
    }
    map
}

/// Extract a plain identifier-ish string from a setting atom: a string literal `"ids"`,
/// or an implicit field `.id`.
fn str_atom(e: &Expr) -> Option<String> {
    match e {
        Expr::ImplicitField(n) => Some(n.node.clone()),
        Expr::Str { parts, .. } => {
            let mut s = String::new();
            for p in parts {
                if let StrPart::Lit(t) = p {
                    s.push_str(t);
                }
            }
            Some(s)
        }
        _ => None,
    }
}

/// Stamp each GET fetch to a batch-enabled endpoint with a candidate [`BatchSpec`]
/// (empty ids); the optimizer fuses groups of candidates. Recurses into `match` arms.
fn annotate_batch(body: &mut Body, batch: &std::collections::HashMap<String, (String, String)>) {
    for node in &mut body.nodes {
        match &mut node.kind {
            NodeKind::Fetch(f) if f.method == "GET" => {
                if let Some((param, key)) = batch.get(&f.endpoint) {
                    f.batch = Some(BatchSpec {
                        query_param: param.clone(),
                        key_field: key.clone(),
                        ids: Vec::new(),
                        mapped: None,
                    });
                }
            }
            NodeKind::Match(m) => {
                for arm in &mut m.arms {
                    if let ArmBodyIr::Body(b) = &mut arm.body {
                        annotate_batch(b, batch);
                    }
                }
            }
            NodeKind::ForEach(fe) => annotate_batch(&mut fe.body, batch),
            _ => {}
        }
    }
}

/// Read each endpoint's declared `latency: <duration>` (ms) from the program.
fn endpoint_latencies(program: &Program) -> std::collections::HashMap<String, u64> {
    let mut map = std::collections::HashMap::new();
    for item in &program.items {
        if let Item::Endpoint(e) = item {
            if let Some(s) = e.settings.iter().find(|s| s.key.node == "latency") {
                if let Some(Expr::Duration(ms, _)) = s.values.first() {
                    map.insert(e.name.node.clone(), *ms);
                }
            }
        }
    }
    map
}

/// Stamp `FetchIr::latency_ms` from the endpoint map, recursing into `match`-arm bodies.
fn annotate_latency(body: &mut Body, latency: &std::collections::HashMap<String, u64>) {
    for node in &mut body.nodes {
        match &mut node.kind {
            NodeKind::Fetch(f) => f.latency_ms = latency.get(&f.endpoint).copied(),
            NodeKind::Match(m) => {
                for arm in &mut m.arms {
                    if let ArmBodyIr::Body(b) = &mut arm.body {
                        annotate_latency(b, latency);
                    }
                }
            }
            NodeKind::ForEach(fe) => annotate_latency(&mut fe.body, latency),
            _ => {}
        }
    }
}

fn lower_server(decl: &ServerDecl) -> Server {
    let mut base = String::new();
    let mut port = None;
    for s in &decl.settings {
        match s.key.node.as_str() {
            "base" => {
                if let Some(Expr::Str { parts, .. }) = s.values.first() {
                    base = parts
                        .iter()
                        .filter_map(|p| match p {
                            StrPart::Lit(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect();
                }
            }
            "port" => {
                if let Some(Expr::Int(n, _)) = s.values.first() {
                    port = Some((*n).clamp(1, 65535) as u16);
                }
            }
            _ => {}
        }
    }
    let routes = decl
        .routes
        .iter()
        .map(|r| Route {
            method: r.method.node.to_uppercase(),
            path: r.path.clone(),
            param_names: r
                .path
                .segments
                .iter()
                .filter_map(|seg| match seg {
                    PathSeg::Param(Expr::Ident(n)) => Some(n.node.clone()),
                    _ => None,
                })
                .collect(),
            budget: r.budget.clone(),
            body: lower_stmts(&r.handler.stmts),
        })
        .collect();
    Server {
        name: decl.name.node.clone(),
        base,
        port,
        routes,
    }
}

fn collect_mocks(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::UsingMock { name, .. } => Some(name.node.clone()),
            _ => None,
        })
        .collect()
}

/// Lower a statement list into a [`Body`], flattening `parallel { }` blocks and
/// dropping `using mock` statements (handled out of band by the test runner).
pub fn lower_stmts(stmts: &[Stmt]) -> Body {
    let mut nodes: Vec<Node> = Vec::new();
    for s in stmts {
        lower_stmt(s, &mut nodes);
    }
    compute_deps(&mut nodes);
    Body {
        nodes,
        waves: Vec::new(),
    }
}

fn lower_stmt(s: &Stmt, nodes: &mut Vec<Node>) {
    match s {
        Stmt::UsingMock { .. } => {}
        Stmt::Parallel { block, .. } => {
            for inner in &block.stmts {
                lower_stmt(inner, nodes);
            }
        }
        Stmt::ForEach {
            var,
            iter,
            body,
            span,
        } => {
            let inner = lower_stmts(&body.stmts);
            // The loop reads the iterable plus whatever its body refers to from the
            // enclosing scope (so the loop depends on the node producing the collection).
            let mut reads = vars(iter);
            let mut external = body_external_reads(&inner);
            external.remove(&var.node); // the loop variable is bound by the loop itself
            reads.extend(external);
            let returns = body_can_return(&inner);
            push(
                nodes,
                NodeKind::ForEach(Box::new(ForEachIr {
                    var: var.node.clone(),
                    iter: iter.clone(),
                    body: inner,
                    returns,
                    span: *span,
                })),
                None,
                reads,
                true, // a loop is an effect: ordered, always live (it does I/O)
                *span,
            );
        }
        Stmt::Fetch(f) => {
            let mut reads = BTreeSet::new();
            for seg in &f.path.segments {
                if let PathSeg::Param(e) = seg {
                    free_vars(e, &mut reads);
                }
            }
            for (_, v) in &f.params {
                free_vars(v, &mut reads);
            }
            if let Some(b) = &f.body {
                free_vars(b, &mut reads);
            }
            if let Some(k) = &f.idempotency_key {
                free_vars(k, &mut reads);
            }
            for op in &f.pipeline {
                pipeline_reads(op, &mut reads);
            }
            let as_result = matches!(
                f.bind.as_ref().and_then(|b| b.ty.as_ref()),
                Some(TypeExpr::Generic(n, _)) if n == "Result"
            );
            // A mutating request (anything but GET) is a side effect: it must keep its
            // order, must never be deduplicated, and must never be eliminated even if its
            // result is unused. We mark it as an effect node to get exactly that.
            let is_mutation = f.method != "GET";
            push(
                nodes,
                NodeKind::Fetch(Box::new(FetchIr {
                    method: f.method.clone(),
                    endpoint: f.endpoint.node.clone(),
                    endpoint_span: f.endpoint.span,
                    path: f.path.clone(),
                    params: f
                        .params
                        .iter()
                        .map(|(k, v)| (k.node.clone(), v.clone()))
                        .collect(),
                    body: f.body.clone(),
                    idempotency_key: f.idempotency_key.clone(),
                    latency_ms: None, // filled in by `annotate_latency` once endpoints are known
                    batch: None,      // filled in by `annotate_batch`
                    pipeline: f.pipeline.clone(),
                    as_result,
                    contract_ty: f.bind.as_ref().and_then(|b| b.ty.clone()),
                })),
                f.bind.as_ref().map(|b| b.name.node.clone()),
                reads,
                is_mutation,
                f.span,
            );
        }
        Stmt::Let { name, value, span } => {
            let reads = vars(value);
            push(
                nodes,
                NodeKind::Let(value.clone()),
                Some(name.node.clone()),
                reads,
                false,
                *span,
            );
        }
        Stmt::Log { value, span } => {
            let reads = vars(value);
            push(
                nodes,
                NodeKind::Log(value.clone()),
                None,
                reads,
                true,
                *span,
            );
        }
        Stmt::Return { value, span } => {
            let reads = value.as_ref().map(vars).unwrap_or_default();
            push(
                nodes,
                NodeKind::Return(value.clone()),
                None,
                reads,
                true,
                *span,
            );
        }
        Stmt::Assert { value, span } => {
            let reads = vars(value);
            push(
                nodes,
                NodeKind::Assert(value.clone()),
                None,
                reads,
                true,
                *span,
            );
        }
        Stmt::Expr { expr, bind, span } => {
            let reads = vars(expr);
            push(
                nodes,
                NodeKind::Expr(expr.clone()),
                bind.as_ref().map(|b| b.name.node.clone()),
                reads,
                true,
                *span,
            );
        }
    }
}

fn lower_match(m: &MatchExpr) -> MatchIr {
    let arms = m
        .arms
        .iter()
        .map(|a| ArmIr {
            pattern: a.pattern.clone(),
            body: match &a.body {
                ArmBody::Value(e) => ArmBodyIr::Value(e.clone()),
                ArmBody::Block(b) => ArmBodyIr::Body(lower_stmts(&b.stmts)),
                ArmBody::Retry { effects, .. } => ArmBodyIr::Retry {
                    effects: effects.clone(),
                },
            },
        })
        .collect();
    MatchIr {
        scrutinee: m.scrutinee.clone(),
        arms,
        span: m.span,
    }
}

#[allow(clippy::too_many_arguments)]
fn push(
    nodes: &mut Vec<Node>,
    mut kind: NodeKind,
    binding: Option<String>,
    reads: BTreeSet<String>,
    effect: bool,
    span: Span,
) {
    // A statement-level expression that is a `match` becomes a Match node, whose reads
    // include the externally-referenced variables of its arm bodies.
    let mut all_reads = reads;
    if let NodeKind::Expr(Expr::Match(m)) = &kind {
        let mir = lower_match(m);
        let mut r = BTreeSet::new();
        free_vars(&mir.scrutinee, &mut r);
        for arm in &mir.arms {
            arm_external_reads(arm, &mut r);
        }
        all_reads = r;
        kind = NodeKind::Match(mir);
    }
    let id = nodes.len();
    nodes.push(Node {
        id,
        kind,
        binding,
        reads: all_reads.into_iter().collect(),
        deps: Vec::new(),
        effect,
        live: true,
        span,
    });
}

/// Wire dependency edges: a node depends on the latest earlier writer of each variable
/// it reads, and each effect depends on the previous effect (to preserve ordering).
fn compute_deps(nodes: &mut [Node]) {
    use std::collections::HashMap;
    let mut last_writer: HashMap<String, NodeId> = HashMap::new();
    let mut last_effect: Option<NodeId> = None;
    // Indexed loop: we read `nodes[i].reads` and write `nodes[i].deps` on the same slice.
    #[allow(clippy::needless_range_loop)]
    for i in 0..nodes.len() {
        let mut deps: BTreeSet<NodeId> = BTreeSet::new();
        for r in &nodes[i].reads {
            if let Some(&w) = last_writer.get(r) {
                deps.insert(w);
            }
        }
        if nodes[i].effect {
            if let Some(e) = last_effect {
                deps.insert(e);
            }
            last_effect = Some(i);
        }
        nodes[i].deps = deps.into_iter().collect();
        if let Some(b) = nodes[i].binding.clone() {
            last_writer.insert(b, i);
        }
    }
}

// ---------- free-variable analysis ----------

fn vars(e: &Expr) -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    free_vars(e, &mut s);
    s
}

/// Free variables of an expression — used by the optimizer's fusion pass.
pub(crate) fn free_vars_of(e: &Expr) -> BTreeSet<String> {
    vars(e)
}

fn pipeline_reads(op: &PipelineOp, out: &mut BTreeSet<String>) {
    match op {
        PipelineOp::Filter { lambda, .. } | PipelineOp::Map { lambda, .. } => {
            free_vars(lambda, out)
        }
        PipelineOp::Sort { by, .. } => free_vars(by, out),
        PipelineOp::Limit { count, .. } | PipelineOp::Skip { count, .. } => free_vars(count, out),
        PipelineOp::Unique { by: Some(e), .. } | PipelineOp::Sum { by: Some(e), .. } => {
            free_vars(e, out)
        }
        PipelineOp::Reverse { .. }
        | PipelineOp::Flatten { .. }
        | PipelineOp::Count { .. }
        | PipelineOp::Unique { by: None, .. }
        | PipelineOp::Sum { by: None, .. } => {}
    }
}

/// Collect the free variables of an expression. Lambda parameters are bound locally and
/// excluded; constructors/field names are not treated as variables.
fn free_vars(e: &Expr, out: &mut BTreeSet<String>) {
    match e {
        Expr::Ident(n) => {
            out.insert(n.node.clone());
        }
        Expr::EnvVar(_)
        | Expr::Int(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::Null(_)
        | Expr::Duration(..)
        | Expr::ImplicitField(_) => {}
        Expr::Str { parts, .. } => {
            for p in parts {
                if let StrPart::Interp(ex) = p {
                    free_vars(ex, out);
                }
            }
        }
        Expr::Field { base, .. } => free_vars(base, out),
        Expr::Call { callee, args, .. } => {
            free_vars(callee, out);
            for a in args {
                free_vars(a, out);
            }
        }
        Expr::Unary { rhs, .. } => free_vars(rhs, out),
        Expr::Binary { lhs, rhs, .. } => {
            free_vars(lhs, out);
            free_vars(rhs, out);
        }
        Expr::Record { fields, .. } => {
            for (_, v) in fields {
                free_vars(v, out);
            }
        }
        Expr::Array { elems, .. } => {
            for x in elems {
                free_vars(x, out);
            }
        }
        Expr::Spread { expr, .. } => free_vars(expr, out),
        Expr::Lambda { param, body, .. } => {
            let mut inner = BTreeSet::new();
            free_vars(body, &mut inner);
            inner.remove(&param.node);
            out.extend(inner);
        }
        Expr::Match(m) => {
            free_vars(&m.scrutinee, out);
            for arm in &m.arms {
                let mut inner = BTreeSet::new();
                match &arm.body {
                    ArmBody::Value(e) => free_vars(e, &mut inner),
                    ArmBody::Block(b) => {
                        for s in &b.stmts {
                            stmt_free_vars(s, &mut inner);
                        }
                    }
                    ArmBody::Retry { effects, .. } => {
                        for eff in effects {
                            match eff {
                                Effect::Call(e) | Effect::Wait(e) => free_vars(e, &mut inner),
                            }
                        }
                    }
                }
                for b in pattern_binds(&arm.pattern) {
                    inner.remove(&b);
                }
                out.extend(inner);
            }
        }
        Expr::Range { lo, hi, .. } => {
            free_vars(lo, out);
            free_vars(hi, out);
        }
    }
}

fn stmt_free_vars(s: &Stmt, out: &mut BTreeSet<String>) {
    match s {
        Stmt::Fetch(f) => {
            for seg in &f.path.segments {
                if let PathSeg::Param(e) = seg {
                    free_vars(e, out);
                }
            }
            for (_, v) in &f.params {
                free_vars(v, out);
            }
            for op in &f.pipeline {
                pipeline_reads(op, out);
            }
        }
        Stmt::Let { value, .. } => free_vars(value, out),
        Stmt::Log { value, .. } | Stmt::Assert { value, .. } => free_vars(value, out),
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                free_vars(v, out);
            }
        }
        Stmt::Expr { expr, .. } => free_vars(expr, out),
        Stmt::Parallel { block, .. } => {
            for s in &block.stmts {
                stmt_free_vars(s, out);
            }
        }
        Stmt::ForEach {
            var, iter, body, ..
        } => {
            free_vars(iter, out);
            let mut inner = BTreeSet::new();
            for s in &body.stmts {
                stmt_free_vars(s, &mut inner);
            }
            inner.remove(&var.node);
            out.extend(inner);
        }
        Stmt::UsingMock { .. } => {}
    }
}

/// The variables a [`Body`] reads from its enclosing scope: every node's reads minus the
/// variables that body binds itself. Used to wire a `for`/match node's dependency edges.
fn body_external_reads(body: &Body) -> BTreeSet<String> {
    let mut reads = BTreeSet::new();
    let mut binds = BTreeSet::new();
    for n in &body.nodes {
        for r in &n.reads {
            reads.insert(r.clone());
        }
        if let Some(b) = &n.binding {
            binds.insert(b.clone());
        }
    }
    for b in binds {
        reads.remove(&b);
    }
    reads
}

/// Whether a body can hit a `return` — directly, or inside a nested `match` arm or `for`
/// loop. Lets the executor distinguish an early return from a trailing body value.
fn body_can_return(body: &Body) -> bool {
    body.nodes.iter().any(|n| match &n.kind {
        NodeKind::Return(_) => true,
        NodeKind::ForEach(fe) => fe.returns,
        NodeKind::Match(m) => m.arms.iter().any(|a| match &a.body {
            ArmBodyIr::Body(b) => body_can_return(b),
            _ => false,
        }),
        _ => false,
    })
}

fn arm_external_reads(arm: &ArmIr, out: &mut BTreeSet<String>) {
    let mut inner = BTreeSet::new();
    let mut local_binds = BTreeSet::new();
    match &arm.body {
        ArmBodyIr::Value(e) => free_vars(e, &mut inner),
        ArmBodyIr::Retry { effects } => {
            for eff in effects {
                match eff {
                    Effect::Call(e) | Effect::Wait(e) => free_vars(e, &mut inner),
                }
            }
        }
        ArmBodyIr::Body(body) => {
            for n in &body.nodes {
                for r in &n.reads {
                    inner.insert(r.clone());
                }
                if let Some(b) = &n.binding {
                    local_binds.insert(b.clone());
                }
            }
        }
    }
    for b in local_binds {
        inner.remove(&b);
    }
    for b in pattern_binds(&arm.pattern) {
        inner.remove(&b);
    }
    out.extend(inner);
}

fn pattern_binds(p: &Pattern) -> Vec<String> {
    let mut v = Vec::new();
    collect_pattern_binds(p, &mut v);
    v
}

fn collect_pattern_binds(p: &Pattern, out: &mut Vec<String>) {
    match p {
        Pattern::Binding(n) => out.push(n.node.clone()),
        Pattern::Wildcard(_) => {}
        Pattern::Ctor { args, .. } => {
            for a in args {
                collect_pattern_binds(a, out);
            }
        }
    }
}
