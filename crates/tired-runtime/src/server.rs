//! `server` mode — the other half of the loop. A `server { route ... }` declaration is
//! served over HTTP here: each request is matched to a route, the path params (plus
//! `query` and `body`) are bound, and the route's handler runs through the **same
//! executor** as everything else. So a handler that fans out to several upstreams is
//! parallelized, deduplicated and dead-request-eliminated automatically — an API gateway
//! whose concurrency the compiler writes for you.
//!
//! The HTTP/1.1 server is hand-rolled on tokio (no extra dependency).

use std::collections::HashMap;
use std::sync::Arc;

use tired_compiler::ir::Server;
use tired_syntax::ast::PathSeg;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::eval::Env;
use crate::value::{from_json, RunError, Value};
use crate::Shared;

/// Serve a named `server` block until the process is stopped.
pub(crate) async fn serve(
    shared: Arc<Shared>,
    name: &str,
    port_override: Option<u16>,
) -> Result<(), RunError> {
    let server = shared
        .compiled
        .server(name)
        .ok_or_else(|| RunError::new(format!("no `server {name}` declared")))?
        .clone();
    let server = Arc::new(server);

    let port = port_override.or(server.port).unwrap_or(8080);
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| RunError::new(format!("cannot bind {addr}: {e}")))?;

    eprintln!(
        "TIRED server `{name}` listening on http://{addr}{}",
        server.base
    );
    for r in &server.routes {
        eprintln!(
            "  {} {}{}",
            r.method,
            server.base,
            tired_compiler::ir::render_path(&r.path)
        );
    }

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => continue,
        };
        let shared = shared.clone();
        let server = server.clone();
        tokio::spawn(async move {
            let _ = handle(stream, shared, server).await;
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    shared: Arc<Shared>,
    server: Arc<Server>,
) -> std::io::Result<()> {
    let Some((method, target, body_bytes)) = read_request(&mut stream).await else {
        return write_response(&mut stream, 400, r#"{"error":"bad request"}"#).await;
    };

    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (target.clone(), Vec::new()),
    };

    // Strip the server's base prefix.
    let path = match raw_path.strip_prefix(&server.base) {
        Some(rest) if !server.base.is_empty() => rest.to_string(),
        _ if server.base.is_empty() => raw_path.clone(),
        _ => {
            return write_response(&mut stream, 404, r#"{"error":"not found"}"#).await;
        }
    };
    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    for route in &server.routes {
        if route.method != method.to_uppercase() || route.path.segments.len() != segments.len() {
            continue;
        }
        if let Some(params) = match_route(route, &segments) {
            // Build the handler environment.
            let mut env: Env = HashMap::new();
            for (k, v) in params {
                env.insert(k, Value::Str(v));
            }
            env.insert("query".to_string(), query_object(&query));
            env.insert("body".to_string(), body_value(&body_bytes));

            let active = std::collections::HashSet::new();
            let result = shared.run_body(&route.body, &mut env, &active).await;
            return match result {
                Ok(Some(v)) => write_response(&mut stream, 200, &v.to_json()).await,
                Ok(None) => write_response(&mut stream, 200, "null").await,
                Err(e) => {
                    let msg = serde_json::json!({ "error": e.message }).to_string();
                    write_response(&mut stream, 500, &msg).await
                }
            };
        }
    }
    write_response(&mut stream, 404, r#"{"error":"no matching route"}"#).await
}

fn match_route(
    route: &tired_compiler::ir::Route,
    segments: &[&str],
) -> Option<Vec<(String, String)>> {
    let mut params = Vec::new();
    let mut pi = 0;
    for (seg, part) in route.path.segments.iter().zip(segments) {
        match seg {
            PathSeg::Literal(l) => {
                if l != part {
                    return None;
                }
            }
            PathSeg::Param(_) => {
                let name = route
                    .param_names
                    .get(pi)
                    .cloned()
                    .unwrap_or_else(|| format!("_{pi}"));
                pi += 1;
                params.push((name, (*part).to_string()));
            }
        }
    }
    Some(params)
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| match kv.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (kv.to_string(), String::new()),
        })
        .collect()
}

fn query_object(query: &[(String, String)]) -> Value {
    let mut o = std::collections::BTreeMap::new();
    for (k, v) in query {
        o.insert(k.clone(), Value::Str(v.clone()));
    }
    Value::Object(o)
}

fn body_value(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(j) => from_json(&j),
        Err(_) => Value::Str(String::from_utf8_lossy(bytes).into_owned()),
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut content_len = 0usize;
    for l in lines {
        if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
            content_len = v.trim().parse().unwrap_or(0);
        }
    }

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some((method, target, body))
}

async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes()).await?;
    stream.flush().await
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
