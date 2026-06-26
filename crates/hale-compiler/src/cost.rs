//! Static request-cost analysis — something a plain HTTP client cannot do for you.
//!
//! Walking the optimized IR, we compute, for a flow / route / script:
//!   * `max_requests` — an upper bound on how many network requests any execution path
//!     can issue (a `match` contributes the *max* over its arms; a flow call adds that
//!     flow's cost; recursion is broken; a `retry` arm adds a bounded number of re-fetches);
//!   * `max_parallel` — the widest fan-out (most requests issued concurrently in one wave);
//!   * `depth` — the **critical-path hops**: the number of *sequential* request rounds on
//!     the longest dependency chain. Independent requests in one wave count as a single
//!     hop, so this is the dominant factor in end-to-end latency.
//!
//! `hale explain` surfaces all three, and [`check_budgets`] turns a declared
//! `budget(requests: N, parallel: K, hops: M)` into a **compile-time SLA**: if the worst
//! case exceeds the budget, the program does not compile.

use hale_syntax::ast::{Budget, Expr};
use hale_syntax::diag::{Diagnostic, Diagnostics};

use crate::ir::{render_path, ArmBodyIr, Body, Flow, NodeKind, Server};

/// Bound on `retry` re-issues (mirrors the runtime's cap).
const RETRY_BUDGET: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub max_requests: usize,
    pub max_parallel: usize,
    /// Sequential request round-trips on the critical path.
    pub depth: usize,
}

impl Cost {
    /// Alternative composition (`match` arms — only one runs): everything is the max.
    fn join_alt(self, other: Cost) -> Cost {
        Cost {
            max_requests: self.max_requests.max(other.max_requests),
            max_parallel: self.max_parallel.max(other.max_parallel),
            depth: self.depth.max(other.depth),
        }
    }
}

/// Worst-case request cost of a body.
pub fn request_cost(body: &Body, flows: &[Flow]) -> Cost {
    body_cost(body, flows, &mut Vec::new())
}

fn body_cost(body: &Body, flows: &[Flow], visiting: &mut Vec<String>) -> Cost {
    // The intrinsic cost of each node (a fetch is one request / one hop; a flow call is
    // the called flow's cost; a `match` is the max over its arms).
    let mut node_costs: Vec<Cost> = vec![Cost::default(); body.nodes.len()];
    for node in &body.nodes {
        if !node.live {
            continue;
        }
        node_costs[node.id] = match &node.kind {
            NodeKind::Fetch(_) => Cost {
                max_requests: 1,
                max_parallel: 0,
                depth: 1,
            },
            NodeKind::Expr(e) => flow_call_cost(e, flows, visiting).unwrap_or_default(),
            NodeKind::Match(m) => m.arms.iter().fold(Cost::default(), |acc, arm| {
                acc.join_alt(arm_cost(&arm.body, flows, visiting))
            }),
            _ => Cost::default(),
        };
    }

    let mut cost = Cost::default();
    // Total requests is schedule-independent: sum every node's intrinsic requests.
    for c in &node_costs {
        cost.max_requests += c.max_requests;
        cost.max_parallel = cost.max_parallel.max(c.max_parallel);
    }
    // Parallel width and critical-path depth come from the wave schedule: requests in one
    // wave are concurrent (count toward `max_parallel`, one shared hop of `depth`); waves
    // are sequential (their depths add).
    for wave in &body.waves {
        let fetches = wave
            .iter()
            .filter(|&&id| body.nodes[id].kind.is_fetch())
            .count();
        cost.max_parallel = cost.max_parallel.max(fetches);
        let hop = wave
            .iter()
            .map(|&id| node_costs[id].depth)
            .max()
            .unwrap_or(0);
        cost.depth += hop;
    }
    cost
}

fn arm_cost(body: &ArmBodyIr, flows: &[Flow], visiting: &mut Vec<String>) -> Cost {
    match body {
        ArmBodyIr::Body(b) => body_cost(b, flows, visiting),
        ArmBodyIr::Value(_) => Cost::default(), // value arms evaluate synchronously
        ArmBodyIr::Retry { .. } => Cost {
            max_requests: RETRY_BUDGET,
            max_parallel: 1,
            depth: RETRY_BUDGET, // retries are sequential
        },
    }
}

fn flow_call_cost(e: &Expr, flows: &[Flow], visiting: &mut Vec<String>) -> Option<Cost> {
    let name = match e {
        Expr::Call { callee, .. } => match callee.as_ref() {
            Expr::Ident(n) => n.node.clone(),
            _ => return None,
        },
        _ => return None,
    };
    if visiting.contains(&name) {
        return Some(Cost::default()); // break recursion
    }
    let flow = flows.iter().find(|f| f.name == name)?;
    visiting.push(name);
    let c = body_cost(&flow.body, flows, visiting);
    visiting.pop();
    Some(c)
}

// ---------- compile-time SLA enforcement ----------

/// Check every declared `budget(...)` against the worst-case cost analysis. A flow or
/// route that can exceed its declared request / parallelism / hop budget is a hard error —
/// the SLA is part of the program's type, not a runtime hope.
pub fn check_budgets(flows: &[Flow], servers: &[Server]) -> Diagnostics {
    let mut diags = Diagnostics::new();
    for f in flows {
        if let Some(b) = &f.budget {
            let cost = request_cost(&f.body, flows);
            check_one(&format!("flow `{}`", f.name), b, cost, &mut diags);
        }
    }
    for s in servers {
        for r in &s.routes {
            if let Some(b) = &r.budget {
                let cost = request_cost(&r.body, flows);
                let label = format!("route `{} {}`", r.method, render_path(&r.path));
                check_one(&label, b, cost, &mut diags);
            }
        }
    }
    diags
}

fn check_one(what: &str, budget: &Budget, cost: Cost, diags: &mut Diagnostics) {
    if let Some(max) = budget.requests {
        if cost.max_requests as u64 > max {
            diags.push(
                Diagnostic::error(
                    budget.span,
                    format!(
                        "{what} can issue up to {} requests, over its budget of {max}",
                        cost.max_requests
                    ),
                )
                .with_help(
                    "reduce the fan-out (dedup, drop unused fetches, push calls behind a `match`) \
                     or raise `budget(requests: N)`",
                )
                .with_note("static request-cost analysis: this is the worst case over every path"),
            );
        }
    }
    if let Some(max) = budget.parallel {
        if cost.max_parallel as u64 > max {
            diags.push(
                Diagnostic::error(
                    budget.span,
                    format!(
                        "{what} fans out to {} concurrent requests, over its budget of {max}",
                        cost.max_parallel
                    ),
                )
                .with_help(
                    "serialize some calls (add a data dependency) or raise `budget(parallel: K)`",
                )
                .with_note("static request-cost analysis: the widest concurrent wave"),
            );
        }
    }
    if let Some(max) = budget.hops {
        if cost.depth as u64 > max {
            diags.push(
                Diagnostic::error(
                    budget.span,
                    format!(
                        "{what} has a critical path of {} sequential hops, over its budget of {max}",
                        cost.depth
                    ),
                )
                .with_help(
                    "remove a data dependency so more calls run in one wave, or raise `budget(hops: M)`",
                )
                .with_note(
                    "static request-cost analysis: sequential round-trips dominate latency",
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::lower_program;
    use crate::optimize::optimize;

    fn cost_of(src: &str) -> Cost {
        let (prog, d) = hale_syntax::parse(src);
        assert!(!d.has_errors(), "{}", d.render(src, "t"));
        let (mut main, mut flows, mut tests, mut servers) = lower_program(&prog);
        optimize(&mut main, &mut flows, &mut tests, &mut servers);
        request_cost(&main, &flows)
    }

    fn budget_errors(src: &str) -> Vec<String> {
        let (prog, d) = hale_syntax::parse(src);
        assert!(!d.has_errors(), "{}", d.render(src, "t"));
        let (mut main, mut flows, mut tests, mut servers) = lower_program(&prog);
        optimize(&mut main, &mut flows, &mut tests, &mut servers);
        check_budgets(&flows, &servers)
            .items()
            .iter()
            .map(|d| d.message.clone())
            .collect()
    }

    #[test]
    fn counts_parallel_fetches() {
        let c = cost_of(
            r#"endpoint A { base: "x" }
               fetch A /one -> a
               fetch A /two -> b
               fetch A /three -> c
               log "{a} {b} {c}""#,
        );
        assert_eq!(c.max_requests, 3);
        assert_eq!(c.max_parallel, 3);
        // All three are independent → one parallel wave → a single sequential hop.
        assert_eq!(c.depth, 1);
    }

    #[test]
    fn dependent_chain_has_depth() {
        let c = cost_of(
            r#"endpoint A { base: "x" }
               fetch A /user -> user
               fetch A /repos/{user.id} -> repos
               log "{repos}""#,
        );
        assert_eq!(c.max_requests, 2);
        assert_eq!(c.max_parallel, 1);
        assert_eq!(c.depth, 2); // two sequential round-trips
    }

    #[test]
    fn dedup_lowers_the_cost() {
        let c = cost_of(
            r#"endpoint A { base: "x" }
               fetch A /same -> a
               fetch A /same -> b
               log "{a} {b}""#,
        );
        assert_eq!(c.max_requests, 1);
    }

    #[test]
    fn match_takes_the_max_arm_not_the_sum() {
        let c = cost_of(
            r#"endpoint A { base: "x" }
               fetch A /x -> r: Result<Thing, ApiError>
               match r {
                 Ok(v) => v
                 Err(e) => fallback()
               }"#,
        );
        assert_eq!(c.max_requests, 1);
    }

    #[test]
    fn budget_violation_is_reported() {
        let e = budget_errors(
            r#"endpoint A { base: "x" }
               flow F() budget(requests: 1) {
                 fetch A /one -> a
                 fetch A /two -> b
                 log "{a} {b}"
               }"#,
        );
        assert!(
            e.iter().any(|m| m.contains("over its budget of 1")),
            "{e:?}"
        );
    }

    #[test]
    fn budget_within_bounds_is_ok() {
        let e = budget_errors(
            r#"endpoint A { base: "x" }
               flow F() budget(requests: 3, parallel: 3, hops: 1) {
                 fetch A /one -> a
                 fetch A /two -> b
                 log "{a} {b}"
               }"#,
        );
        assert!(e.is_empty(), "{e:?}");
    }

    #[test]
    fn hops_budget_catches_a_long_critical_path() {
        let e = budget_errors(
            r#"endpoint A { base: "x" }
               flow F() budget(hops: 1) {
                 fetch A /user -> user
                 fetch A /repos/{user.id} -> repos
                 log "{repos}"
               }"#,
        );
        assert!(
            e.iter()
                .any(|m| m.contains("critical path") && m.contains("2 sequential hops")),
            "{e:?}"
        );
    }
}
