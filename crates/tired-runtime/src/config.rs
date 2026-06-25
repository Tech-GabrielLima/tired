//! Turning an `endpoint { ... }` declaration into a runtime configuration: base URL,
//! auth scheme, timeout, retry/backoff policy and cache TTL. Parsing is lenient —
//! unknown settings are ignored rather than rejected.

use tired_syntax::ast::{EndpointDecl, Expr, StrPart};

#[derive(Clone, Debug)]
pub struct EndpointConfig {
    pub base: String,
    pub auth: Option<Auth>,
    pub timeout_ms: Option<u64>,
    pub retries: u32,
    pub backoff: Backoff,
    pub cache_ttl_ms: Option<u64>,
    pub trace: bool,
    pub metrics: bool,
}

#[derive(Clone, Debug)]
pub enum Auth {
    /// `Authorization: Bearer <value>` where the value comes from an env var.
    Bearer(String),
    /// A custom header carrying an API key from an env var.
    ApiKey { header: String, env: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backoff {
    Constant,
    Exponential,
}

impl EndpointConfig {
    pub fn from_decl(decl: &EndpointDecl) -> Self {
        let mut cfg = EndpointConfig {
            base: String::new(),
            auth: None,
            timeout_ms: None,
            retries: 0,
            backoff: Backoff::Exponential,
            cache_ttl_ms: None,
            trace: false,
            metrics: false,
        };
        for s in &decl.settings {
            let vals = &s.values;
            match s.key.node.as_str() {
                "base" => {
                    if let Some(b) = vals.first().and_then(string_lit) {
                        cfg.base = b;
                    }
                }
                "auth" => cfg.auth = vals.first().and_then(parse_auth),
                "timeout" => cfg.timeout_ms = vals.first().and_then(duration_ms),
                "retry" => {
                    if let Some(Expr::Int(n, _)) = vals.first() {
                        cfg.retries = (*n).max(0) as u32;
                    }
                    for v in vals {
                        if let Expr::Call { callee, args, .. } = v {
                            if call_name(callee) == Some("backoff") {
                                if let Some(Expr::Ident(k)) = args.first() {
                                    cfg.backoff = match k.node.as_str() {
                                        "constant" => Backoff::Constant,
                                        _ => Backoff::Exponential,
                                    };
                                }
                            }
                        }
                    }
                }
                "cache" => {
                    if let Some(Expr::Call { callee, args, .. }) = vals.first() {
                        if call_name(callee) == Some("ttl") {
                            cfg.cache_ttl_ms = args.first().and_then(duration_ms);
                        }
                    }
                }
                "trace" => cfg.trace = true,
                "metrics" => cfg.metrics = true,
                _ => {}
            }
        }
        cfg
    }
}

fn string_lit(e: &Expr) -> Option<String> {
    if let Expr::Str { parts, .. } = e {
        let mut s = String::new();
        for p in parts {
            if let StrPart::Lit(t) = p {
                s.push_str(t);
            }
        }
        Some(s)
    } else {
        None
    }
}

fn duration_ms(e: &Expr) -> Option<u64> {
    match e {
        Expr::Duration(ms, _) => Some(*ms),
        Expr::Int(n, _) => Some((*n).max(0) as u64),
        _ => None,
    }
}

fn call_name(callee: &Expr) -> Option<&str> {
    match callee {
        Expr::Ident(n) => Some(n.node.as_str()),
        _ => None,
    }
}

fn parse_auth(e: &Expr) -> Option<Auth> {
    if let Expr::Call { callee, args, .. } = e {
        match call_name(callee)? {
            "Bearer" | "Token" => {
                if let Some(Expr::EnvVar(n)) = args.first() {
                    return Some(Auth::Bearer(n.node.clone()));
                }
            }
            "ApiKey" => {
                if let Some(Expr::EnvVar(n)) = args.first() {
                    return Some(Auth::ApiKey {
                        header: "X-Api-Key".into(),
                        env: n.node.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    None
}
