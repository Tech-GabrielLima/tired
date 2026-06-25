//! The optimizer — TIRED's two flagship passes:
//!
//! * **Dead-request elimination.** A `fetch` whose result is never observed (directly
//!   or transitively) is removed and reported. Zero bytes leave the machine for it.
//! * **Parallel inference.** Within a body the nodes form a dependency DAG; we group
//!   them into topological *waves*. Every node in a wave is independent of the others,
//!   so the executor runs a whole wave concurrently — turning sequentially-written
//!   fetches into a parallel schedule without the programmer asking.

use tired_syntax::diag::{Diagnostic, Diagnostics};

use crate::ir::*;

/// Optimize every body in the program in place, returning warnings (e.g. eliminated
/// requests). Recurses into `match` arm bodies.
pub fn optimize(main: &mut Body, flows: &mut [Flow], tests: &mut [Test]) -> Diagnostics {
    let mut diags = Diagnostics::new();
    optimize_body(main, &mut diags);
    for f in flows.iter_mut() {
        optimize_body(&mut f.body, &mut diags);
    }
    for t in tests.iter_mut() {
        optimize_body(&mut t.body, &mut diags);
    }
    diags
}

fn optimize_body(body: &mut Body, diags: &mut Diagnostics) {
    liveness(body, diags);
    schedule_waves(body);
    // Recurse into the nested bodies that live inside match arms.
    for node in &mut body.nodes {
        if let NodeKind::Match(m) = &mut node.kind {
            for arm in &mut m.arms {
                if let ArmBodyIr::Body(b) = &mut arm.body {
                    optimize_body(b, diags);
                }
            }
        }
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

// ---------- execution-plan rendering (`tired explain` / `--show-plan`) ----------

/// Render a human-readable execution plan showing the inferred parallel waves.
pub fn render_plan(main: &Body, flows: &[Flow], tests: &[Test]) -> String {
    let mut out = String::new();
    out.push_str("execution plan (inferred parallelism)\n");
    out.push_str("=====================================\n");
    if !main.nodes.is_empty() {
        out.push_str("\nmain:\n");
        render_body_plan(&mut out, main, 1);
    }
    for f in flows {
        out.push_str(&format!("\nflow {}({}):\n", f.name, f.params.join(", ")));
        render_body_plan(&mut out, &f.body, 1);
    }
    for t in tests {
        out.push_str(&format!("\ntest {:?}:\n", t.description));
        render_body_plan(&mut out, &t.body, 1);
    }
    out
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
            if let NodeKind::Match(m) = &node.kind {
                for arm in &m.arms {
                    if let ArmBodyIr::Body(b) = &arm.body {
                        render_body_plan(out, b, indent + 2);
                    }
                }
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
        let (prog, d) = tired_syntax::parse(src);
        assert!(!d.has_errors(), "parse error: {}", d.render(src, "t"));
        let (mut main, mut flows, mut tests) = lower_program(&prog);
        let diags = optimize(&mut main, &mut flows, &mut tests);
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
            render_plan(&main, &[], &[])
        );
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
            render_plan(&main, &[], &[])
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
