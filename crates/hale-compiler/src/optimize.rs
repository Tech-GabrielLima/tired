//! The optimizer — hale's flagship passes:
//!
//! * **Request deduplication (CSE).** Two `fetch`es that issue the *identical* request
//!   (same endpoint, path, params, pipeline — and the same inputs) are collapsed: the
//!   later one is rewritten to reuse the first's result. The same URL is never hit twice.
//! * **Dead-request elimination.** A `fetch` whose result is never observed (directly
//!   or transitively) is removed and reported. Zero bytes leave the machine for it.
//! * **Parallel inference.** Within a body the nodes form a dependency DAG; we group
//!   them into topological *waves*. Every node in a wave is independent of the others,
//!   so the executor runs a whole wave concurrently — turning sequentially-written
//!   fetches into a parallel schedule without the programmer asking.

use hale_syntax::ast::{Expr, PathPattern, PathSeg};
use hale_syntax::diag::{Diagnostic, Diagnostics};
use hale_syntax::pretty;
use hale_syntax::span::Spanned;

use crate::ir::*;

/// Optimize every body in the program in place, returning warnings (e.g. eliminated
/// requests). Recurses into `match` arm bodies.
pub fn optimize(
    main: &mut Body,
    flows: &mut [Flow],
    tests: &mut [Test],
    servers: &mut [Server],
) -> Diagnostics {
    let mut diags = Diagnostics::new();
    // A monotonic counter for the fresh bindings loop fusion introduces (`__loopbatch_N`).
    // Threaded through every body so the names are unique program-wide — nested bodies share
    // the runtime environment, so a per-body counter could collide.
    let mut seq = 0usize;
    optimize_body(main, &mut diags, &mut seq);
    for f in flows.iter_mut() {
        optimize_body(&mut f.body, &mut diags, &mut seq);
    }
    for t in tests.iter_mut() {
        optimize_body(&mut t.body, &mut diags, &mut seq);
    }
    for s in servers.iter_mut() {
        for r in s.routes.iter_mut() {
            optimize_body(&mut r.body, &mut diags, &mut seq);
        }
    }
    diags
}

fn optimize_body(body: &mut Body, diags: &mut Diagnostics, seq: &mut usize) {
    fuse_loops(body, diags, seq);
    dedup_requests(body, diags);
    fuse_requests(body, diags);
    liveness(body, diags);
    schedule_waves(body);
    // Recurse into the nested bodies that live inside match arms and `for` loops.
    for node in &mut body.nodes {
        match &mut node.kind {
            NodeKind::Match(m) => {
                for arm in &mut m.arms {
                    if let ArmBodyIr::Body(b) = &mut arm.body {
                        optimize_body(b, diags, seq);
                    }
                }
            }
            NodeKind::ForEach(fe) => optimize_body(&mut fe.body, diags, seq),
            _ => {}
        }
    }
}

/// Collapse identical requests. A fetch whose request signature (endpoint + path +
/// params + pipeline + the producers of its inputs) matches an earlier fetch is rewritten
/// to simply reuse the earlier binding — so the network sees the request only once.
fn dedup_requests(body: &mut Body, diags: &mut Diagnostics) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, (NodeId, String)> = HashMap::new();
    // (alias node id, first node id, first binding name)
    let mut rewrites: Vec<(NodeId, NodeId, String)> = Vec::new();

    for node in &body.nodes {
        if let NodeKind::Fetch(f) = &node.kind {
            // Only GET is safe to deduplicate — a mutation must always be sent.
            if f.method != "GET" {
                continue;
            }
            // A loop-fused (mapped) batch's request depends on its collection, which the
            // signature does not capture — never dedup it against another batched call.
            if f.batch.as_ref().is_some_and(|b| b.mapped.is_some()) {
                continue;
            }
            let sig = fetch_sig(f, &node.reads, &node.deps);
            match seen.get(&sig) {
                Some((first_id, first_bind)) => {
                    if node.binding.is_some() {
                        rewrites.push((node.id, *first_id, first_bind.clone()));
                    }
                }
                None => {
                    if let Some(bind) = &node.binding {
                        seen.insert(sig, (node.id, bind.clone()));
                    }
                }
            }
        }
    }

    for (alias_id, first_id, first_bind) in rewrites {
        let (endpoint, path) = match &body.nodes[alias_id].kind {
            NodeKind::Fetch(f) => (f.endpoint.clone(), render_path(&f.path)),
            _ => continue,
        };
        diags.push(
            Diagnostic::warning(
                body.nodes[alias_id].span,
                format!("duplicate request `{endpoint} {path}` reuses an identical earlier call"),
            )
            .with_note("request deduplication: the network is hit once; the second call is free"),
        );
        let span = body.nodes[alias_id].span;
        body.nodes[alias_id].kind =
            NodeKind::Let(Expr::Ident(Spanned::new(first_bind.clone(), span)));
        body.nodes[alias_id].reads = vec![first_bind];
        body.nodes[alias_id].deps = vec![first_id];
    }
}

/// A request signature that is equal iff two fetches would issue the exact same request
/// with the exact same inputs (same dependency producers).
fn fetch_sig(f: &FetchIr, reads: &[String], deps: &[NodeId]) -> String {
    let mut s = format!("GET {}", f.endpoint);
    for seg in &f.path.segments {
        s.push('/');
        match seg {
            PathSeg::Literal(l) => s.push_str(l),
            PathSeg::Param(e) => {
                s.push('{');
                s.push_str(&pretty::expr(e));
                s.push('}');
            }
        }
    }
    let mut params: Vec<String> = f
        .params
        .iter()
        .map(|(k, e)| format!("{k}={}", pretty::expr(e)))
        .collect();
    params.sort();
    s.push('?');
    s.push_str(&params.join("&"));
    for op in &f.pipeline {
        s.push('|');
        s.push_str(&pretty::pipeline_op(op));
    }
    let mut r = reads.to_vec();
    r.sort();
    let mut d: Vec<String> = deps.iter().map(|x| x.to_string()).collect();
    d.sort();
    format!("{s}#reads[{}]#deps[{}]", r.join(","), d.join(","))
}

// ---------- request fusion / batching ----------

/// Collapse a group of near-identical GETs that differ only in the last path segment into a
/// **single batched request** plus per-binding *scatter* nodes — when their endpoint declares
/// a `batch:` rule. `fetch GH /users/1`, `/users/2`, `/users/3` become one `GET /users?ids=1,2,3`
/// and three scatters that pick each element out by the join key. Vectorization, for the wire.
fn fuse_requests(body: &mut Body, diags: &mut Diagnostics) {
    use std::collections::{HashMap, HashSet};

    // Group candidate fetches by (endpoint + collection prefix), in first-appearance order.
    let mut order: Vec<String> = Vec::new();
    let mut members: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, node) in body.nodes.iter().enumerate() {
        if let Some(k) = batch_group_key(node) {
            members
                .entry(k.clone())
                .or_insert_with(|| {
                    order.push(k.clone());
                    Vec::new()
                })
                .push(i);
        }
    }

    // node index -> (group id, is_leader); plus per-group metadata for building nodes.
    let mut plan: HashMap<usize, (usize, bool)> = HashMap::new();
    let mut groups: Vec<GroupPlan> = Vec::new();
    for key in &order {
        let m = &members[key];
        if m.len() < 2 {
            continue; // a lone candidate gains nothing from batching
        }
        let first = m[0];
        // Sound placement: the batch is emitted at the first member's position, so every
        // member's id value must be available there (not produced at/after `first`).
        let forbidden: HashSet<&str> = body.nodes[first..]
            .iter()
            .filter_map(|n| n.binding.as_deref())
            .collect();
        let value_of = |idx: usize| -> Expr {
            match &body.nodes[idx].kind {
                NodeKind::Fetch(f) => varying_value(&f.path),
                _ => unreachable!(),
            }
        };
        let sound = m.iter().all(|&idx| {
            crate::lower::free_vars_of(&value_of(idx))
                .iter()
                .all(|v| !forbidden.contains(v.as_str()))
        });
        if !sound {
            continue;
        }

        let leader = &body.nodes[first];
        let NodeKind::Fetch(lf) = &leader.kind else {
            continue;
        };
        let spec = lf.batch.clone().expect("candidate has a batch rule");
        let ids: Vec<Expr> = m.iter().map(|&idx| value_of(idx)).collect();
        let gid = groups.len();
        for (j, &idx) in m.iter().enumerate() {
            plan.insert(idx, (gid, j == 0));
        }
        diags.push(
            Diagnostic::warning(
                leader.span,
                format!(
                    "{} requests to `{} {}` fused into one batched call `?{}=…`",
                    m.len(),
                    lf.endpoint,
                    render_path(&lf.path),
                    spec.query_param
                ),
            )
            .with_note("request fusion: N round-trips collapse to 1; results are scattered back"),
        );
        groups.push(GroupPlan {
            batch_binding: format!("__batch_{gid}"),
            template: (**lf).clone(),
            prefix: prefix_path(&lf.path),
            param: spec.query_param,
            key: spec.key_field,
            ids,
            span: leader.span,
        });
    }
    if groups.is_empty() {
        return;
    }

    // Rebuild the node list: a group's leader expands to [batch fetch, scatter]; later
    // members expand to [scatter]; everything else passes through. Ids/deps recomputed.
    let mut new_nodes: Vec<Node> = Vec::new();
    for (i, node) in body.nodes.iter().enumerate() {
        match plan.get(&i) {
            None => {
                let mut n = node.clone();
                n.id = new_nodes.len();
                new_nodes.push(n);
            }
            Some((gid, is_leader)) => {
                let g = &groups[*gid];
                if *is_leader {
                    new_nodes.push(make_batch_node(new_nodes.len(), g));
                }
                new_nodes.push(make_scatter_node(new_nodes.len(), g, node));
            }
        }
    }
    recompute_deps(&mut new_nodes);
    body.nodes = new_nodes;
}

struct GroupPlan {
    batch_binding: String,
    template: FetchIr,
    prefix: PathPattern,
    param: String,
    key: String,
    ids: Vec<Expr>,
    span: hale_syntax::span::Span,
}

/// A fusable candidate's group key (`endpoint|prefix`), or `None` if it can't be batched.
fn batch_group_key(node: &Node) -> Option<String> {
    let NodeKind::Fetch(f) = &node.kind else {
        return None;
    };
    let spec = f.batch.as_ref()?;
    let batchable = f.method == "GET"
        && spec.ids.is_empty()
        && spec.mapped.is_none()
        && f.params.is_empty()
        && f.pipeline.is_empty()
        && f.idempotency_key.is_none()
        && node.binding.is_some()
        && f.path.segments.len() >= 2;
    if !batchable {
        return None;
    }
    let prefix: Vec<String> = f.path.segments[..f.path.segments.len() - 1]
        .iter()
        .map(|s| match s {
            PathSeg::Literal(l) => l.clone(),
            PathSeg::Param(e) => format!("{{{}}}", pretty::expr(e)),
        })
        .collect();
    Some(format!("{}|/{}", f.endpoint, prefix.join("/")))
}

/// The last path segment as an expression — the value being batched / scattered on.
fn varying_value(p: &PathPattern) -> Expr {
    match p.segments.last() {
        Some(PathSeg::Param(e)) => e.clone(),
        Some(PathSeg::Literal(l)) => match l.parse::<i64>() {
            Ok(n) => Expr::Int(n, p.span),
            Err(_) => Expr::Str {
                parts: vec![hale_syntax::ast::StrPart::Lit(l.clone())],
                span: p.span,
            },
        },
        None => Expr::Null(p.span),
    }
}

/// Drop the last path segment, yielding the collection prefix (e.g. `/users/{x}` → `/users`).
fn prefix_path(p: &PathPattern) -> PathPattern {
    let n = p.segments.len().saturating_sub(1);
    PathPattern {
        segments: p.segments[..n].to_vec(),
        span: p.span,
    }
}

fn make_batch_node(id: NodeId, g: &GroupPlan) -> Node {
    let mut reads = std::collections::BTreeSet::new();
    for e in &g.ids {
        reads.extend(crate::lower::free_vars_of(e));
    }
    let mut f = g.template.clone();
    f.path = g.prefix.clone();
    f.params = Vec::new();
    f.pipeline = Vec::new();
    f.idempotency_key = None;
    f.batch = Some(BatchSpec {
        query_param: g.param.clone(),
        key_field: g.key.clone(),
        ids: g.ids.clone(),
        mapped: None,
    });
    Node {
        id,
        kind: NodeKind::Fetch(Box::new(f)),
        binding: Some(g.batch_binding.clone()),
        reads: reads.into_iter().collect(),
        deps: Vec::new(),
        effect: false,
        live: true,
        span: g.span,
    }
}

fn make_scatter_node(id: NodeId, g: &GroupPlan, original: &Node) -> Node {
    let value = match &original.kind {
        NodeKind::Fetch(f) => varying_value(&f.path),
        _ => unreachable!(),
    };
    let mut reads = std::collections::BTreeSet::new();
    reads.insert(g.batch_binding.clone());
    reads.extend(crate::lower::free_vars_of(&value));
    Node {
        id,
        kind: NodeKind::Scatter(ScatterIr {
            batch: g.batch_binding.clone(),
            key_field: g.key.clone(),
            value,
        }),
        binding: original.binding.clone(),
        reads: reads.into_iter().collect(),
        deps: Vec::new(),
        effect: false,
        live: true,
        span: original.span,
    }
}

/// Recompute dependency edges after fusion rewrote the node list (mirrors lowering's pass:
/// each node depends on the latest earlier writer of a variable it reads, and effects chain).
fn recompute_deps(nodes: &mut [Node]) {
    use std::collections::{BTreeSet, HashMap};
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

// ---------- loop fusion (the N+1 cure) ----------

/// **Loop fusion** — turn an N+1 into a 1+1. A per-element GET to a batch-enabled endpoint
/// inside a `for` loop is *hoisted out* of the loop: all of its keys are gathered into one
/// batched call (`/coll?ids=…`, the ids produced by mapping the key expression over the
/// collection), and the in-loop fetch is replaced by a pure [`NodeKind::Scatter`] that picks
/// each element back out by the join key. It is loop-invariant code motion + batching, for the
/// network — the dynamic-loop counterpart of [`fuse_requests`]'s static fusion.
fn fuse_loops(body: &mut Body, diags: &mut Diagnostics, seq: &mut usize) {
    use std::collections::BTreeSet;

    if !body
        .nodes
        .iter()
        .any(|n| matches!(&n.kind, NodeKind::ForEach(fe) if !loop_fusions(fe).is_empty()))
    {
        return;
    }

    let mut new_nodes: Vec<Node> = Vec::new();
    for node in body.nodes.iter() {
        let NodeKind::ForEach(fe_box) = &node.kind else {
            let mut n = node.clone();
            n.id = new_nodes.len();
            new_nodes.push(n);
            continue;
        };
        let plans = loop_fusions(fe_box);
        if plans.is_empty() {
            let mut n = node.clone();
            n.id = new_nodes.len();
            new_nodes.push(n);
            continue;
        }

        let mut fe = (**fe_box).clone();
        let mut batch_nodes: Vec<Node> = Vec::new();
        for plan in &plans {
            let batch_name = format!("__loopbatch_{}", *seq);
            *seq += 1;
            batch_nodes.push(make_loop_batch_node(0, &batch_name, &fe, plan));
            fe.body.nodes[plan.fetch_idx] =
                make_loop_scatter(&fe.body.nodes[plan.fetch_idx], &batch_name, plan);
            diags.push(
                Diagnostic::warning(
                    plan.span,
                    format!(
                        "per-element request `{} {}` in a `for {}` loop was fused into one batched call `{}?{}=…`",
                        plan.endpoint,
                        render_path(&plan.orig_path),
                        fe.var,
                        render_path(&plan.prefix),
                        plan.query_param,
                    ),
                )
                .with_note(
                    "N+1 eliminated: the per-element GETs are hoisted into a single up-front batched request and scattered back by the join key",
                ),
            );
        }
        // The loop body now reads the batch bindings instead of issuing fetches; recompute
        // its dependency edges, and the loop node's own read set, before re-scheduling.
        recompute_deps(&mut fe.body.nodes);
        let body_binds: BTreeSet<String> = fe
            .body
            .nodes
            .iter()
            .filter_map(|n| n.binding.clone())
            .collect();
        let mut reads: BTreeSet<String> = crate::lower::free_vars_of(&fe.iter);
        for n in &fe.body.nodes {
            for r in &n.reads {
                reads.insert(r.clone());
            }
        }
        for b in &body_binds {
            reads.remove(b);
        }
        reads.remove(&fe.var);

        for mut bn in batch_nodes {
            bn.id = new_nodes.len();
            new_nodes.push(bn);
        }
        let mut fe_node = node.clone();
        fe_node.id = new_nodes.len();
        fe_node.reads = reads.into_iter().collect();
        fe_node.kind = NodeKind::ForEach(Box::new(fe));
        new_nodes.push(fe_node);
    }
    recompute_deps(&mut new_nodes);
    body.nodes = new_nodes;
}

/// A fusable per-element fetch found inside a `for` loop body.
struct LoopFusePlan {
    fetch_idx: usize,
    endpoint: String,
    endpoint_span: hale_syntax::span::Span,
    latency_ms: Option<u64>,
    /// The full original path (for the diagnostic).
    orig_path: PathPattern,
    /// The collection path (the original path minus its last segment).
    prefix: PathPattern,
    /// The last path segment expression — the per-element key (depends on the loop var).
    key: Expr,
    query_param: String,
    key_field: String,
    binding: String,
    span: hale_syntax::span::Span,
}

/// Find every per-element fetch in a loop body that can be soundly hoisted into a batched
/// call: a GET to a `batch:`-enabled endpoint whose last path segment is the loop element
/// (and whose collection prefix is loop-invariant — uses no loop variable or loop-local).
fn loop_fusions(fe: &ForEachIr) -> Vec<LoopFusePlan> {
    use std::collections::BTreeSet;
    let body_binds: BTreeSet<String> = fe
        .body
        .nodes
        .iter()
        .filter_map(|n| n.binding.clone())
        .collect();
    let mut plans = Vec::new();
    for (i, node) in fe.body.nodes.iter().enumerate() {
        let NodeKind::Fetch(f) = &node.kind else {
            continue;
        };
        let Some(spec) = &f.batch else { continue };
        let candidate = f.method == "GET"
            && spec.ids.is_empty()
            && spec.mapped.is_none()
            && f.params.is_empty()
            && f.pipeline.is_empty()
            && f.idempotency_key.is_none()
            && node.binding.is_some()
            && f.path.segments.len() >= 2;
        if !candidate {
            continue;
        }
        let Some(PathSeg::Param(key)) = f.path.segments.last() else {
            continue;
        };
        let key_fv = crate::lower::free_vars_of(key);
        // Must vary with the element (else it is loop-invariant, not an N+1), and must read
        // nothing bound inside the loop body (so it can be evaluated outside the loop).
        if !key_fv.contains(&fe.var) {
            continue;
        }
        if key_fv
            .iter()
            .any(|v| v != &fe.var && body_binds.contains(v))
        {
            continue;
        }
        let prefix = prefix_path(&f.path);
        let prefix_ok = prefix.segments.iter().all(|s| match s {
            PathSeg::Literal(_) => true,
            PathSeg::Param(e) => crate::lower::free_vars_of(e)
                .iter()
                .all(|v| v != &fe.var && !body_binds.contains(v)),
        });
        if !prefix_ok {
            continue;
        }
        plans.push(LoopFusePlan {
            fetch_idx: i,
            endpoint: f.endpoint.clone(),
            endpoint_span: f.endpoint_span,
            latency_ms: f.latency_ms,
            orig_path: f.path.clone(),
            prefix,
            key: key.clone(),
            query_param: spec.query_param.clone(),
            key_field: spec.key_field.clone(),
            binding: node.binding.clone().unwrap_or_default(),
            span: node.span,
        });
    }
    plans
}

/// The single batched fetch a loop fusion hoists before the loop: `GET /coll?param=<ids>`,
/// the ids produced at runtime by mapping the key over the loop's collection.
fn make_loop_batch_node(id: NodeId, batch_name: &str, fe: &ForEachIr, plan: &LoopFusePlan) -> Node {
    let mut reads = std::collections::BTreeSet::new();
    reads.extend(crate::lower::free_vars_of(&fe.iter));
    for v in crate::lower::free_vars_of(&plan.key) {
        if v != fe.var {
            reads.insert(v);
        }
    }
    let f = FetchIr {
        method: "GET".into(),
        endpoint: plan.endpoint.clone(),
        endpoint_span: plan.endpoint_span,
        path: plan.prefix.clone(),
        params: Vec::new(),
        body: None,
        idempotency_key: None,
        latency_ms: plan.latency_ms,
        batch: Some(BatchSpec {
            query_param: plan.query_param.clone(),
            key_field: plan.key_field.clone(),
            ids: Vec::new(),
            mapped: Some(MappedIds {
                coll: fe.iter.clone(),
                var: fe.var.clone(),
                key: plan.key.clone(),
            }),
        }),
        pipeline: Vec::new(),
        as_result: false,
        contract_ty: None,
    };
    Node {
        id,
        kind: NodeKind::Fetch(Box::new(f)),
        binding: Some(batch_name.to_string()),
        reads: reads.into_iter().collect(),
        deps: Vec::new(),
        effect: false,
        live: true,
        span: plan.span,
    }
}

/// The in-loop fetch's replacement: pull this element out of the batched array by join key.
fn make_loop_scatter(original: &Node, batch_name: &str, plan: &LoopFusePlan) -> Node {
    let mut reads = std::collections::BTreeSet::new();
    reads.insert(batch_name.to_string());
    reads.extend(crate::lower::free_vars_of(&plan.key)); // includes the loop variable
    Node {
        id: original.id,
        kind: NodeKind::Scatter(ScatterIr {
            batch: batch_name.to_string(),
            key_field: plan.key_field.clone(),
            value: plan.key.clone(),
        }),
        binding: Some(plan.binding.clone()),
        reads: reads.into_iter().collect(),
        deps: Vec::new(),
        effect: false,
        live: true,
        span: original.span,
    }
}

/// Backward reachability from observable (effect) nodes. Anything not reached is dead;
/// dead *fetches* are reported since they would otherwise hit the network for nothing.
fn liveness(body: &mut Body, diags: &mut Diagnostics) {
    let n = body.nodes.len();
    let mut live = vec![false; n];
    let mut stack = Vec::new();
    for node in &body.nodes {
        if node.effect && !live[node.id] {
            live[node.id] = true;
            stack.push(node.id);
        }
    }
    while let Some(id) = stack.pop() {
        for &d in &body.nodes[id].deps {
            if !live[d] {
                live[d] = true;
                stack.push(d);
            }
        }
    }
    for node in &mut body.nodes {
        node.live = live[node.id];
        if !node.live {
            if let NodeKind::Fetch(f) = &node.kind {
                diags.push(
                    Diagnostic::warning(
                        node.span,
                        format!(
                            "request `{} {}` is never used and was eliminated",
                            f.endpoint,
                            render_path(&f.path)
                        ),
                    )
                    .with_note("dead-request elimination: 0 bytes were sent for it"),
                );
            }
        }
    }
}

/// Topologically level the live nodes. `level(n) = 1 + max(level(deps))`, and nodes
/// sharing a level form a wave that runs concurrently.
fn schedule_waves(body: &mut Body) {
    let n = body.nodes.len();
    let mut level = vec![0usize; n];
    let mut max_level = 0;
    // Dependencies always point to earlier ids, so a single forward pass suffices.
    for i in 0..n {
        if !body.nodes[i].live {
            continue;
        }
        let mut lvl = 0;
        for &d in &body.nodes[i].deps {
            if body.nodes[d].live {
                lvl = lvl.max(level[d] + 1);
            }
        }
        level[i] = lvl;
        max_level = max_level.max(lvl);
    }
    let mut waves: Vec<Vec<NodeId>> = vec![Vec::new(); max_level + 1];
    for node in &body.nodes {
        if node.live {
            waves[level[node.id]].push(node.id);
        }
    }
    waves.retain(|w| !w.is_empty());
    body.waves = waves;
}

// ---------- execution-plan rendering (`hale explain` / `--show-plan`) ----------

/// Render a human-readable execution plan showing the inferred parallel waves, and the
/// static request-cost of each flow/route (max requests across all paths).
pub fn render_plan(main: &Body, flows: &[Flow], tests: &[Test], servers: &[Server]) -> String {
    let mut out = String::new();
    out.push_str("execution plan (inferred parallelism + request cost)\n");
    out.push_str("====================================================\n");
    if !main.nodes.is_empty() {
        out.push_str(&format!("\nmain:{}\n", cost_suffix(main, flows)));
        render_body_plan(&mut out, main, 1);
    }
    for f in flows {
        out.push_str(&format!(
            "\nflow {}({}):{}{}\n",
            f.name,
            f.params.join(", "),
            cost_suffix(&f.body, flows),
            budget_suffix(f.budget.as_ref())
        ));
        out.push_str(&effect_line(&f.body, flows, 1));
        render_body_plan(&mut out, &f.body, 1);
    }
    for s in servers {
        out.push_str(&format!("\nserver {}:\n", s.name));
        for r in &s.routes {
            out.push_str(&format!(
                "  route {} {}:{}{}\n",
                r.method,
                render_path(&r.path),
                cost_suffix(&r.body, flows),
                budget_suffix(r.budget.as_ref())
            ));
            out.push_str(&effect_line(&r.body, flows, 2));
            render_body_plan(&mut out, &r.body, 2);
        }
    }
    for t in tests {
        out.push_str(&format!("\ntest {:?}:\n", t.description));
        render_body_plan(&mut out, &t.body, 1);
    }
    out
}

/// The composed effect signature (endpoints touched + read/write), as an indented line.
fn effect_line(body: &Body, flows: &[Flow], indent: usize) -> String {
    let e = crate::cost::effects(body, flows);
    if e.endpoints.is_empty() {
        return String::new();
    }
    let pad = "  ".repeat(indent);
    let kind = if e.mutates { "reads+writes" } else { "reads" };
    let eps: Vec<&str> = e.endpoints.iter().map(|s| s.as_str()).collect();
    format!("{pad}effects: {kind} {{{}}}\n", eps.join(", "))
}

fn cost_suffix(body: &Body, flows: &[Flow]) -> String {
    let c = crate::cost::request_cost(body, flows);
    if c.unbounded {
        // A per-element fetch in a loop: the count scales with the collection (N+1).
        return format!(
            "  [unbounded requests — a fetch runs once per `for` element (N+1); {} per iteration, up to {} in parallel]",
            c.max_requests, c.max_parallel,
        );
    }
    let lat = match c.latency_ms {
        Some(ms) if ms > 0 => format!(", ~{ms}ms critical path"),
        _ => String::new(),
    };
    format!(
        "  [≤ {} request{}, up to {} in parallel, {} hop{} deep{}]",
        c.max_requests,
        if c.max_requests == 1 { "" } else { "s" },
        c.max_parallel,
        c.depth,
        if c.depth == 1 { "" } else { "s" },
        lat,
    )
}

/// Render a ` budget(...)` annotation in the plan when a flow/route declares an SLA.
fn budget_suffix(budget: Option<&hale_syntax::ast::Budget>) -> String {
    let Some(b) = budget else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(r) = b.requests {
        parts.push(format!("requests ≤ {r}"));
    }
    if let Some(p) = b.parallel {
        parts.push(format!("parallel ≤ {p}"));
    }
    if let Some(h) = b.hops {
        parts.push(format!("hops ≤ {h}"));
    }
    if let Some(ms) = b.p99_ms {
        parts.push(format!("p99 ≤ {ms}ms"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  (budget: {})", parts.join(", "))
    }
}

fn render_body_plan(out: &mut String, body: &Body, indent: usize) {
    let pad = "  ".repeat(indent);
    if body.waves.is_empty() {
        out.push_str(&format!("{pad}(no live operations)\n"));
        return;
    }
    for (i, wave) in body.waves.iter().enumerate() {
        let fetches = wave
            .iter()
            .filter(|&&id| body.nodes[id].kind.is_fetch())
            .count();
        let tag = if fetches > 1 {
            format!("  ‖ {fetches} requests in parallel")
        } else {
            String::new()
        };
        out.push_str(&format!("{pad}wave {}:{}\n", i + 1, tag));
        for &id in wave {
            let node = &body.nodes[id];
            let bind = node
                .binding
                .as_ref()
                .map(|b| format!(" -> {b}"))
                .unwrap_or_default();
            out.push_str(&format!("{pad}  • {}{}\n", node.kind.label(), bind));
            match &node.kind {
                NodeKind::Match(m) => {
                    for arm in &m.arms {
                        if let ArmBodyIr::Body(b) = &arm.body {
                            render_body_plan(out, b, indent + 2);
                        }
                    }
                }
                NodeKind::ForEach(fe) => render_body_plan(out, &fe.body, indent + 2),
                _ => {}
            }
        }
    }
}

/// Summary statistics used by the CLI to report how much parallelism was inferred.
pub struct PlanStats {
    pub total_fetches: usize,
    pub fetch_waves: usize,
    pub max_parallel: usize,
}

pub fn body_stats(body: &Body) -> PlanStats {
    let mut total = 0;
    let mut fetch_waves = 0;
    let mut max_parallel = 0;
    for wave in &body.waves {
        let f = wave
            .iter()
            .filter(|&&id| body.nodes[id].kind.is_fetch())
            .count();
        total += f;
        if f > 0 {
            fetch_waves += 1;
        }
        max_parallel = max_parallel.max(f);
    }
    PlanStats {
        total_fetches: total,
        fetch_waves,
        max_parallel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::lower_program;

    fn compile(src: &str) -> (Body, Vec<Flow>, Vec<Test>, Diagnostics) {
        let (prog, d) = hale_syntax::parse(src);
        assert!(!d.has_errors(), "parse error: {}", d.render(src, "t"));
        let (mut main, mut flows, mut tests, mut servers) = lower_program(&prog);
        let diags = optimize(&mut main, &mut flows, &mut tests, &mut servers);
        (main, flows, tests, diags)
    }

    #[test]
    fn independent_fetches_are_parallelized() {
        let src = r#"
            endpoint A { base: "x" }
            fetch A /one   -> a
            fetch A /two   -> b
            fetch A /three -> c
            log "{a} {b} {c}"
        "#;
        let (main, _f, _t, _d) = compile(src);
        let stats = body_stats(&main);
        assert_eq!(stats.total_fetches, 3);
        // All three are independent → they collapse into a single parallel wave.
        assert_eq!(
            stats.max_parallel,
            3,
            "plan: {}",
            render_plan(&main, &[], &[], &[])
        );
    }

    #[test]
    fn identical_requests_are_deduplicated() {
        let src = r#"
            endpoint A { base: "x" }
            fetch A /users/gabriel -> a
            fetch A /users/gabriel -> b
            log "{a} {b}"
        "#;
        let (main, _f, _t, diags) = compile(src);
        let live_fetches = main
            .nodes
            .iter()
            .filter(|n| n.live && n.kind.is_fetch())
            .count();
        assert_eq!(
            live_fetches,
            1,
            "plan: {}",
            render_plan(&main, &[], &[], &[])
        );
        let warns: Vec<_> = diags.items().iter().map(|d| d.message.clone()).collect();
        assert!(
            warns.iter().any(|m| m.contains("duplicate request")),
            "{warns:?}"
        );
    }

    #[test]
    fn distinct_requests_are_not_deduplicated() {
        let src = r#"
            endpoint A { base: "x" }
            fetch A /users/a -> a
            fetch A /users/b -> b
            log "{a} {b}"
        "#;
        let (main, _f, _t, _d) = compile(src);
        let live_fetches = main
            .nodes
            .iter()
            .filter(|n| n.live && n.kind.is_fetch())
            .count();
        assert_eq!(live_fetches, 2);
    }

    #[test]
    fn dependent_fetches_are_serialized() {
        let src = r#"
            endpoint A { base: "x" }
            fetch A /user -> user
            fetch A /repos/{user.id} -> repos
            log "{repos}"
        "#;
        let (main, _f, _t, _d) = compile(src);
        let stats = body_stats(&main);
        assert_eq!(
            stats.max_parallel,
            1,
            "plan: {}",
            render_plan(&main, &[], &[], &[])
        );
    }

    #[test]
    fn batchable_gets_are_fused() {
        let src = r#"
            endpoint GH { base: "x"  batch: param("ids") key(.id) }
            fetch GH /users/1 -> a
            fetch GH /users/2 -> b
            fetch GH /users/3 -> c
            log "{a} {b} {c}"
        "#;
        let (main, _f, _t, _d) = compile(src);
        let fetches = main
            .nodes
            .iter()
            .filter(|n| n.live && n.kind.is_fetch())
            .count();
        let scatters = main
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Scatter(_)))
            .count();
        assert_eq!(
            fetches,
            1,
            "three GETs should fuse to one batched call\nplan: {}",
            render_plan(&main, &[], &[], &[])
        );
        assert_eq!(scatters, 3, "one scatter per original binding");
        // The blast radius collapses from three requests to one.
        assert_eq!(crate::cost::request_cost(&main, &[]).max_requests, 1);
    }

    #[test]
    fn distinct_collections_are_not_fused() {
        let src = r#"
            endpoint GH { base: "x"  batch: param("ids") key(.id) }
            fetch GH /users/1 -> a
            fetch GH /orgs/2  -> b
            log "{a} {b}"
        "#;
        let (main, _f, _t, _d) = compile(src);
        let fetches = main
            .nodes
            .iter()
            .filter(|n| n.live && n.kind.is_fetch())
            .count();
        assert_eq!(fetches, 2, "different collections must not be fused");
    }

    #[test]
    fn per_element_loop_fetch_is_fused_into_one_batched_call() {
        let src = r#"
            endpoint GH { base: "x"  batch: param("ids") key(.id) }
            fetch GH /users -> users
            for u in users {
                fetch GH /users/{u.id} -> detail
                log "{detail}"
            }
        "#;
        let (main, _f, _t, diags) = compile(src);

        // The per-element GET is hoisted out of the loop into ONE mapped batched call.
        let batched = main
            .nodes
            .iter()
            .filter(|n| {
                matches!(&n.kind, NodeKind::Fetch(f) if f.batch.as_ref().is_some_and(|b| b.mapped.is_some()))
            })
            .count();
        assert_eq!(
            batched,
            1,
            "expected one mapped batched fetch\nplan: {}",
            render_plan(&main, &[], &[], &[])
        );

        // Inside the loop the fetch is gone, replaced by a pure scatter.
        let fe = main
            .nodes
            .iter()
            .find_map(|n| match &n.kind {
                NodeKind::ForEach(fe) => Some(fe),
                _ => None,
            })
            .expect("a for loop");
        assert_eq!(
            fe.body.nodes.iter().filter(|n| n.kind.is_fetch()).count(),
            0,
            "the per-element fetch should be hoisted out"
        );
        assert_eq!(
            fe.body
                .nodes
                .iter()
                .filter(|n| matches!(n.kind, NodeKind::Scatter(_)))
                .count(),
            1,
            "replaced by a scatter"
        );

        // The N+1 is gone: the cost is now bounded (the list + one batched call).
        let c = crate::cost::request_cost(&main, &[]);
        assert!(!c.unbounded, "fusion must make the cost bounded: {c:?}");
        assert_eq!(c.max_requests, 2);

        let warns: Vec<_> = diags.items().iter().map(|d| d.message.clone()).collect();
        assert!(
            warns
                .iter()
                .any(|m| m.contains("fused into one batched call")),
            "{warns:?}"
        );
    }

    #[test]
    fn a_loop_fetch_with_a_param_prefix_is_not_fused() {
        // The collection prefix varies per element, so the calls hit different collections —
        // they cannot collapse into one batched request.
        let src = r#"
            endpoint GH { base: "x"  batch: param("ids") key(.id) }
            fetch GH /users -> users
            for u in users {
                fetch GH /tenants/{u.tenant}/users/{u.id} -> detail
                log "{detail}"
            }
        "#;
        let (main, _f, _t, _d) = compile(src);
        let batched = main
            .nodes
            .iter()
            .filter(|n| {
                matches!(&n.kind, NodeKind::Fetch(f) if f.batch.as_ref().is_some_and(|b| b.mapped.is_some()))
            })
            .count();
        assert_eq!(
            batched, 0,
            "a per-element collection prefix must block fusion"
        );
    }

    #[test]
    fn dead_request_is_eliminated() {
        let src = r#"
            endpoint A { base: "x" }
            fetch A /used   -> a
            fetch A /unused -> b
            log "{a}"
        "#;
        let (main, _f, _t, diags) = compile(src);
        let warns: Vec<_> = diags.items().iter().map(|d| d.message.clone()).collect();
        assert!(warns.iter().any(|m| m.contains("never used")), "{warns:?}");
        // The dead fetch is excluded from the schedule.
        let scheduled: usize = main.waves.iter().map(|w| w.len()).sum();
        let live_fetches = main
            .nodes
            .iter()
            .filter(|n| n.live && n.kind.is_fetch())
            .count();
        assert_eq!(live_fetches, 1);
        assert!(scheduled >= 1);
    }
}
