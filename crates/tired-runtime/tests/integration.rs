//! End-to-end runtime tests against a tiny in-process HTTP/1.1 server. The server
//! counts requests and sleeps a fixed latency per request, which lets us assert two
//! things deterministically and offline:
//!   * **parallel inference** actually overlaps independent fetches (wall clock ≪ sum);
//!   * **dead-request elimination** sends zero bytes for an unused fetch.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tired_compiler::compile;
use tired_runtime::{Runtime, Value};

/// A minimal HTTP/1.1 test server. `latency` is slept on every request; `count` tracks
/// how many requests were served. The body is chosen by path.
struct TestServer {
    base: String,
    count: Arc<AtomicUsize>,
}

fn spawn_server(latency: Duration) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let count = count2.clone();
            thread::spawn(move || handle(stream, latency, count));
        }
    });
    TestServer {
        base: format!("http://127.0.0.1:{port}"),
        count,
    }
}

fn handle(mut stream: TcpStream, latency: Duration, count: Arc<AtomicUsize>) {
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    count.fetch_add(1, Ordering::SeqCst);
    thread::sleep(latency);

    let (status, body) = response_for(&path);
    let res = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(res.as_bytes());
    let _ = stream.flush();
}

fn response_for(path: &str) -> (&'static str, String) {
    if path.contains("/repos") {
        // Note the negative star count — used by the contract test.
        (
            "200 OK",
            r#"[{"id":1,"name":"alpha","stars":-5}]"#.to_string(),
        )
    } else if path.ends_with("/missing") {
        ("404 Not Found", r#"{"error":"not found"}"#.to_string())
    } else {
        let login = path.rsplit('/').next().unwrap_or("user");
        ("200 OK", format!(r#"{{"login":"{login}","id":7}}"#))
    }
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Minimal blocking HTTP/1.1 GET for driving the TIRED server under test.
fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

fn build(src: &str) -> Runtime {
    let (compiled, diags) = compile(src, "test.tired");
    assert!(
        !diags.has_errors(),
        "compile errors: {}",
        diags.render(src, "test.tired")
    );
    Runtime::new(compiled.expect("compiled"))
}

#[test]
fn parallel_inference_overlaps_independent_fetches() {
    let server = spawn_server(Duration::from_millis(200));
    let src = format!(
        r#"
        endpoint S {{ base: "{}" timeout: 5s }}
        fetch S /users/a -> a
        fetch S /users/b -> b
        fetch S /users/c -> c
        log "{{a.login}} {{b.login}} {{c.login}}"
        "#,
        server.base
    );
    let runtime = build(&src);

    let start = Instant::now();
    rt().block_on(async { runtime.run().await }).expect("run");
    let elapsed = start.elapsed();

    assert_eq!(server.count.load(Ordering::SeqCst), 3);
    // Serial would be ~600ms; concurrent should finish well under that.
    assert!(
        elapsed < Duration::from_millis(450),
        "expected concurrent execution, took {elapsed:?}"
    );
    println!("3×200ms fetches finished in {elapsed:?} (serial lower bound: 600ms)");
}

#[test]
fn duplicate_requests_hit_the_network_once() {
    let server = spawn_server(Duration::from_millis(5));
    let src = format!(
        r#"
        endpoint S {{ base: "{}" }}
        fetch S /users/gabriel -> a
        fetch S /users/gabriel -> b
        log "{{a.login}} {{b.login}}"
        "#,
        server.base
    );
    let runtime = build(&src);
    rt().block_on(async { runtime.run().await }).expect("run");
    // Both bindings are used, but the identical request is deduplicated → 1 network call.
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
}

#[test]
fn mutations_are_always_sent_never_deduped_or_eliminated() {
    let server = spawn_server(Duration::from_millis(5));
    // Two identical POSTs (both results unused) + one unused GET.
    let src = format!(
        r#"
        endpoint S {{ base: "{}" }}
        fetch POST S /orders body {{ item: 1 }}
        fetch POST S /orders body {{ item: 1 }}
        fetch S /unused -> g
        log "done"
        "#,
        server.base
    );
    let runtime = build(&src);
    rt().block_on(async { runtime.run().await }).expect("run");
    // Both POSTs are sent (mutations are never deduplicated or eliminated); the unused
    // GET is dead-request-eliminated. So exactly 2 requests reach the network.
    assert_eq!(server.count.load(Ordering::SeqCst), 2);
}

#[test]
fn dead_request_is_never_sent() {
    let server = spawn_server(Duration::from_millis(10));
    let src = format!(
        r#"
        endpoint S {{ base: "{}" }}
        fetch S /users/used   -> a
        fetch S /users/unused -> b
        log "{{a.login}}"
        "#,
        server.base
    );
    let runtime = build(&src);
    rt().block_on(async { runtime.run().await }).expect("run");
    // Only the `used` fetch should ever hit the wire.
    assert_eq!(server.count.load(Ordering::SeqCst), 1);
}

#[test]
fn contract_violation_is_caught_at_runtime() {
    let server = spawn_server(Duration::from_millis(5));
    // `stars >= 0` is violated by the server's `-5`.
    let src = format!(
        r#"
        endpoint S {{ base: "{}" }}
        contract Repo {{ id: Integer  name: String  stars: Integer where (>= 0) }}
        fetch S /users/x/repos -> repos: Repo[]
        log "{{repos.length}}"
        "#,
        server.base
    );
    let runtime = build(&src);
    let result = rt().block_on(async { runtime.run().await });
    let err = result.expect_err("contract violation should abort");
    assert!(err.message.contains("contract"), "got: {}", err.message);
    assert!(err.message.contains("stars"), "got: {}", err.message);
}

/// Prints a parallel-vs-serial comparison. Run with:
/// `cargo test -p tired-runtime --test integration benchmark -- --nocapture`
///
/// Honesty note: this measures the *engine* against an in-process server with a fixed
/// per-request latency injected in software — it characterises how the scheduler
/// overlaps requests, not a production network. The serial figure is a genuine baseline
/// (the same fetches chained by a data dependency, forcing one wave each).
#[test]
fn benchmark_parallel_vs_serial() {
    let hop = Duration::from_millis(100);
    let n = 6;

    // Independent fetches → one parallel wave.
    let server = spawn_server(hop);
    let mut prog = format!("endpoint S {{ base: \"{}\" timeout: 5s }}\n", server.base);
    for i in 0..n {
        prog.push_str(&format!("fetch S /users/u{i} -> v{i}\n"));
    }
    // Reference every result so none is eliminated as a dead request.
    let refs: Vec<String> = (0..n).map(|i| format!("{{v{i}.login}}")).collect();
    prog.push_str(&format!("log \"{}\"\n", refs.join(" ")));
    let runtime = build(&prog);
    let t = Instant::now();
    rt().block_on(async { runtime.run().await }).unwrap();
    let parallel = t.elapsed();
    assert_eq!(
        server.count.load(Ordering::SeqCst),
        n,
        "all fetches should run"
    );

    // Chained fetches (each path depends on the previous result) → n serial waves.
    let server2 = spawn_server(hop);
    let mut chain = format!("endpoint S {{ base: \"{}\" timeout: 5s }}\n", server2.base);
    chain.push_str("fetch S /users/start -> v0\n");
    for i in 1..n {
        chain.push_str(&format!("fetch S /users/{{v{}.login}} -> v{i}\n", i - 1));
    }
    chain.push_str(&format!("log \"{{v{}.login}}\"\n", n - 1));
    let runtime2 = build(&chain);
    let t = Instant::now();
    rt().block_on(async { runtime2.run().await }).unwrap();
    let serial = t.elapsed();

    println!("\n=== TIRED parallel-inference benchmark ({n} fetches @ {hop:?}/hop) ===");
    println!("  serial   (data-dependent chain): {serial:?}");
    println!("  parallel (independent, inferred): {parallel:?}");
    println!(
        "  speedup: {:.2}x\n",
        serial.as_secs_f64() / parallel.as_secs_f64()
    );

    assert!(
        parallel < serial,
        "parallel ({parallel:?}) should beat serial ({serial:?})"
    );
}

#[test]
fn server_mode_aggregates_and_parallelizes_upstreams() {
    let upstream = spawn_server(Duration::from_millis(100));
    let port = free_port();
    let src = format!(
        r#"
        endpoint Up {{ base: "{}" }}
        server Gateway {{
          route GET /agg/{{u}} -> {{
            fetch Up /users/{{u}}       -> a
            fetch Up /users/{{u}}/extra -> b
            return {{ user: a.login, extra: b.login }}
          }}
        }}
        "#,
        upstream.base
    );
    let runtime = build(&src);
    let rt = rt();
    rt.spawn(async move {
        let _ = runtime.serve("Gateway", Some(port)).await;
    });
    std::thread::sleep(Duration::from_millis(400)); // let it bind

    let start = Instant::now();
    let (status, body) = http_get(&format!("127.0.0.1:{port}"), "/agg/gabriel");
    let elapsed = start.elapsed();

    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"user\":\"gabriel\""), "body: {body}");
    assert!(body.contains("\"extra\":\"extra\""), "body: {body}");
    assert_eq!(
        upstream.count.load(Ordering::SeqCst),
        2,
        "both upstreams should be called"
    );
    // The two upstream calls (100ms each) ran in parallel inside the handler.
    assert!(
        elapsed < Duration::from_millis(350),
        "handler should parallelize; took {elapsed:?}"
    );
}

#[test]
fn pipeline_and_result_handling_end_to_end() {
    let server = spawn_server(Duration::from_millis(5));
    let src = format!(
        r#"
        endpoint S {{ base: "{}" }}
        flow Lookup(name: String) -> User {{
            fetch S /users/{{name}} -> r: Result<User, ApiError>
            match r {{
                Ok(u)         => u
                Err(NotFound) => fallback()
                Err(e)        => fallback()
            }}
        }}
        Lookup("alice") -> who
        log "{{who.login}}"
        "#,
        server.base
    );
    let runtime = build(&src);
    let out = rt().block_on(async {
        runtime
            .run_flow("Lookup", vec![Value::Str("alice".into())])
            .await
    });
    let user = out.expect("flow ok");
    assert_eq!(user.get_field("login"), Value::Str("alice".into()));
}
