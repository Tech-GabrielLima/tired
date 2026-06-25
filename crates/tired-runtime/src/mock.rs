//! The in-language mock engine. A `mock NAME { GET /users/{id} -> {...} }` block
//! becomes a routing table; a request is matched by method + path, path parameters are
//! captured and exposed to the response body as `$id`, and a response naming an error
//! variant (e.g. `NotFound`) produces a typed failure. Mocks make `tired test` run
//! fully offline and deterministic.

use std::collections::HashMap;

use tired_syntax::ast::{Expr, MockDecl, PathSeg};

use crate::eval::{eval, Env};
use crate::value::{ErrValue, Outcome, Value};

pub struct MockEngine {
    routes: Vec<MockRoute>,
}

struct MockRoute {
    method: String,
    segments: Vec<Seg>,
    response: Expr,
}

enum Seg {
    Lit(String),
    Param(String),
}

impl MockEngine {
    pub fn from_decl(decl: &MockDecl) -> Self {
        let routes = decl
            .routes
            .iter()
            .map(|r| MockRoute {
                method: r.method.node.to_uppercase(),
                segments: r
                    .path
                    .segments
                    .iter()
                    .map(|s| match s {
                        PathSeg::Literal(l) => Seg::Lit(l.clone()),
                        PathSeg::Param(e) => Seg::Param(param_name(e)),
                    })
                    .collect(),
                response: r.response.clone(),
            })
            .collect();
        MockEngine { routes }
    }

    /// Resolve a request against the mock table. Unmatched routes yield `404 NotFound`.
    pub fn lookup(&self, method: &str, path: &str) -> Outcome {
        let parts: Vec<&str> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let method = method.to_uppercase();
        for route in &self.routes {
            if route.method != method || route.segments.len() != parts.len() {
                continue;
            }
            let mut captured: Env = HashMap::new();
            let mut ok = true;
            for (seg, part) in route.segments.iter().zip(&parts) {
                match seg {
                    Seg::Lit(l) => {
                        if l != part {
                            ok = false;
                            break;
                        }
                    }
                    Seg::Param(name) => {
                        captured.insert(name.clone(), Value::Str((*part).to_string()));
                    }
                }
            }
            if ok {
                return self.respond(&route.response, &captured);
            }
        }
        Outcome::Failure(ErrValue::new(
            "NotFound",
            Some(404),
            format!("no mock route for {method} {path}"),
        ))
    }

    fn respond(&self, response: &Expr, env: &Env) -> Outcome {
        // A response that names an error variant becomes a typed failure.
        if let Some(out) = error_response(response, env) {
            return out;
        }
        match eval(response, env, None) {
            Ok(v) => Outcome::Success(v),
            Err(e) => Outcome::Failure(ErrValue::new("MockError", None, e.message)),
        }
    }
}

fn param_name(e: &Expr) -> String {
    match e {
        Expr::Ident(n) => n.node.clone(),
        _ => "_".to_string(),
    }
}

/// Map an HTTP error variant name to its status code.
pub fn error_status(name: &str) -> Option<u16> {
    Some(match name {
        "NotModified" => 304,
        "BadRequest" => 400,
        "Unauthorized" => 401,
        "Forbidden" => 403,
        "NotFound" => 404,
        "Timeout" => 408,
        "Conflict" => 409,
        "RateLimit" | "TooManyRequests" => 429,
        "ServerError" => 500,
        _ => return None,
    })
}

/// If `response` is `NotFound` or `RateLimit(ms)` etc., build the corresponding failure.
fn error_response(response: &Expr, env: &Env) -> Option<Outcome> {
    match response {
        Expr::Ident(n) => {
            let status = error_status(&n.node)?;
            Some(Outcome::Failure(ErrValue::new(
                n.node.clone(),
                Some(status),
                n.node.clone(),
            )))
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(n) = callee.as_ref() {
                let status = error_status(&n.node)?;
                let mut err = ErrValue::new(n.node.clone(), Some(status), n.node.clone());
                if let Some(a) = args.first() {
                    if let Ok(p) = eval(a, env, None) {
                        err = err.with_payload(p);
                    }
                }
                return Some(Outcome::Failure(err));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(src: &str) -> MockEngine {
        let (prog, d) = tired_syntax::parse(src);
        assert!(!d.has_errors(), "{}", d.render(src, "t"));
        for item in &prog.items {
            if let tired_syntax::ast::Item::Mock(m) = item {
                return MockEngine::from_decl(m);
            }
        }
        panic!("no mock");
    }

    #[test]
    fn matches_param_route() {
        let e = engine(r#"mock GH { GET /users/{id} -> { id: $id, login: "gabriel" } }"#);
        match e.lookup("GET", "/users/42") {
            Outcome::Success(v) => {
                assert_eq!(v.get_field("id"), Value::Str("42".into()));
                assert_eq!(v.get_field("login"), Value::Str("gabriel".into()));
            }
            _ => panic!("expected success"),
        }
    }

    #[test]
    fn error_variant_response() {
        let e = engine("mock GH { GET /repos/999 -> NotFound }");
        match e.lookup("GET", "/repos/999") {
            Outcome::Failure(err) => assert_eq!(err.variant, "NotFound"),
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn unmatched_is_not_found() {
        let e = engine("mock GH { GET /a -> { ok: true } }");
        assert!(matches!(e.lookup("GET", "/b"), Outcome::Failure(_)));
    }
}
