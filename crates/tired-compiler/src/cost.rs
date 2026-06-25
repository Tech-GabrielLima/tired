//! Static request-cost analysis — something a plain HTTP client cannot do for you.
//!
//! Walking the optimized IR, we compute, for a flow / route / script, an **upper bound
//! on how many network requests any execution path can issue**, and the **maximum number
//! that run in parallel**. A `match` contributes the *max* over its arms (only one runs);
//! a flow call adds that flow's cost (recursion is broken safely); a `retry` arm adds a
//! bounded number of re-fetches. `tired explain` surfaces it as `[≤ N requests, up to K
//! in parallel]`, so you can see the blast radius of an endpoint before you ship it.

use tired_syntax::ast::Expr;

use crate::ir::{ArmBodyIr, Body, Flow, NodeKind};

/// Bound on `retry` re-issues (mirrors the runtime's cap).
const RETRY_BUDGET: usize = 5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub max_requests: usize,
    pub max_parallel: usize,
}

impl Cost {
    fn join_seq(self, other: Cost) -> Cost {
        Cost {
            max_requests: self.max_requests + other.max_requests,
            max_parallel: self.max_parallel.max(other.max_parallel),
        }
    }
    fn join_alt(self, other: Cost) -> Cost {
        Cost {
            max_requests: self.max_requests.max(other.max_requests),
            max_parallel: self.max_parallel.max(other.max_parallel),
        }
    }
}

/// Worst-case request cost of a body.
pub fn request_cost(body: &Body, flows: &[Flow]) -> Cost {
    body_cost(body, flows, &mut Vec::new())
}

fn body_cost(body: &Body, flows: &[Flow], visiting: &mut Vec<String>) -> Cost {
    let mut cost = Cost::default();

    // Local parallel width: the most fetches in any single wave.
    for wave in &body.waves {
        let fetches = wave
            .iter()
            .filter(|&&id| body.nodes[id].kind.is_fetch())
            .count();
        cost.max_parallel = cost.max_parallel.max(fetches);
    }

    for node in &body.nodes {
        if !node.live {
            continue;
        }
        match &node.kind {
            NodeKind::Fetch(_) => {
                cost = cost.join_seq(Cost {
                    max_requests: 1,
                    max_parallel: 0,
                });
            }
            NodeKind::Expr(e) => {
                if let Some(c) = flow_call_cost(e, flows, visiting) {
                    cost = cost.join_seq(c);
                }
            }
            NodeKind::Match(m) => {
                let mut arms = Cost::default();
                for arm in &m.arms {
                    arms = arms.join_alt(arm_cost(&arm.body, flows, visiting));
                }
                cost = cost.join_seq(arms);
            }
            _ => {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::lower_program;
    use crate::optimize::optimize;

    fn cost_of(src: &str) -> Cost {
        let (prog, d) = tired_syntax::parse(src);
        assert!(!d.has_errors(), "{}", d.render(src, "t"));
        let (mut main, mut flows, mut tests, mut servers) = lower_program(&prog);
        optimize(&mut main, &mut flows, &mut tests, &mut servers);
        request_cost(&main, &flows)
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
    }

    #[test]
    fn dedup_lowers_the_cost() {
        let c = cost_of(
            r#"endpoint A { base: "x" }
               fetch A /same -> a
               fetch A /same -> b
               log "{a} {b}""#,
        );
        // The duplicate is deduplicated away, so only one request can be issued.
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
        // Just the one fetch; the match arms add no further requests.
        assert_eq!(c.max_requests, 1);
    }
}
