//! `tired` — the command-line driver.
//!
//! ```text
//! tired run     <file> [--flow NAME [arg ...]] [--show-plan] [--metrics]
//! tired check   <file>
//! tired fmt     <file> [--write]
//! tired test    <file>
//! tired explain <file>
//! ```

use std::process::ExitCode;

use tired_compiler::compile;
use tired_runtime::{fetch_json, infer, RecordMode, Runtime, Value};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
        return ExitCode::FAILURE;
    }
    match args[0].as_str() {
        "run" => cmd_run(&args[1..]).await,
        "check" => cmd_check(&args[1..]),
        "fmt" => cmd_fmt(&args[1..]),
        "test" => cmd_test(&args[1..]).await,
        "explain" | "plan" => cmd_explain(&args[1..]),
        "inspect" => cmd_inspect(&args[1..]).await,
        "replay" => cmd_replay(&args[1..]).await,
        "schema" => cmd_schema(&args[1..]),
        "serve" => cmd_serve(&args[1..]).await,
        "lsp" => {
            tired_lsp::run();
            ExitCode::SUCCESS
        }
        "--version" | "-V" | "version" => {
            println!("tired {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command `{other}`\n");
            usage();
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "TIRED — a compiled DSL for consuming HTTP APIs\n\n\
         usage:\n\
         \x20 tired run     <file> [--flow NAME [arg ...]] [--show-plan] [--metrics]\n\
         \x20                      [--record <rec.json>] [--replay <rec.json>]\n\
         \x20 tired check   <file>\n\
         \x20 tired fmt     <file> [--write]\n\
         \x20 tired test    <file>\n\
         \x20 tired explain <file>\n\
         \x20 tired inspect <url|file.json> [TypeName]   # infer TIRED types from JSON\n\
         \x20 tired schema  <file>                       # export types/contracts as JSON Schema\n\
         \x20 tired serve   <file> [Server] [--port N]   # run a `server` block over HTTP\n\
         \x20 tired replay  <rec.json> <file>            # re-run offline from a recording\n\
         \x20 tired lsp                                  # run the language server (stdio)"
    );
}

fn read(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("error: cannot read `{path}`: {e}");
            None
        }
    }
}

/// Print all diagnostics; return true if any were hard errors.
fn report(diags: &tired_syntax::Diagnostics, src: &str, path: &str) -> bool {
    if !diags.is_empty() {
        eprint!("{}", diags.render(src, path));
    }
    diags.has_errors()
}

async fn cmd_run(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired run` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let show_plan = args.iter().any(|a| a == "--show-plan");
    let metrics = args.iter().any(|a| a == "--metrics");
    let flow = flag_value(args, "--flow");
    let record_path = flag_value(args, "--record");
    let replay_path = flag_value(args, "--replay");

    let (compiled, diags) = compile(&src, path);
    if report(&diags, &src, path) {
        return ExitCode::FAILURE;
    }
    let Some(compiled) = compiled else {
        return ExitCode::FAILURE;
    };

    let mode = match &replay_path {
        Some(p) => match RecordMode::replay_from(p) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
        None if record_path.is_some() => RecordMode::record(),
        None => RecordMode::Off,
    };
    let rt = Runtime::with_mode(compiled, mode);
    if show_plan {
        println!("{}", rt.plan());
    }

    let result = if let Some(name) = flow {
        let flow_args: Vec<Value> = flow_args(args).into_iter().map(Value::Str).collect();
        rt.run_flow(&name, flow_args).await.map(Some)
    } else {
        rt.run().await
    };

    match result {
        Ok(Some(v)) => {
            if !matches!(v, Value::Null) {
                println!("=> {}", v.display());
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("runtime error: {e}");
            return ExitCode::FAILURE;
        }
    }

    if let Some(p) = &record_path {
        match rt.save_recording(p) {
            Ok(()) => eprintln!("\n[record] saved responses to {p}"),
            Err(e) => eprintln!("\n[record] {e}"),
        }
    }

    if metrics {
        let m = rt.metrics();
        eprintln!(
            "\n[metrics] requests={} cache_hits={} retries={} errors={}",
            m.requests, m.cache_hits, m.retries, m.errors
        );
    }
    ExitCode::SUCCESS
}

/// `tired inspect <url|file.json> [TypeName]` — infer TIRED types from a JSON sample.
async fn cmd_inspect(args: &[String]) -> ExitCode {
    let Some(target) = args.first() else {
        eprintln!("error: `tired inspect` needs a URL or a .json file");
        return ExitCode::FAILURE;
    };
    let name = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "Root".into());

    let json = if target.starts_with("http://") || target.starts_with("https://") {
        match fetch_json(target).await {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: fetching `{target}`: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let Some(text) = read(target) else {
            return ExitCode::FAILURE;
        };
        match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: `{target}` is not valid JSON: {e}");
                return ExitCode::FAILURE;
            }
        }
    };

    print!("{}", infer::infer_types(&json, &name));
    ExitCode::SUCCESS
}

/// `tired serve <file> [Server] [--port N]` — run a `server` block over HTTP.
async fn cmd_serve(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired serve` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let (compiled, diags) = compile(&src, path);
    if report(&diags, &src, path) {
        return ExitCode::FAILURE;
    }
    let Some(compiled) = compiled else {
        return ExitCode::FAILURE;
    };
    let port = flag_value(args, "--port").and_then(|p| p.parse::<u16>().ok());
    let rt = Runtime::new(compiled);
    let names = rt.server_names();
    let name = match args.get(1).filter(|a| !a.starts_with("--")) {
        Some(n) => n.clone(),
        None => match names.first() {
            Some(n) => n.clone(),
            None => {
                eprintln!("no `server` block found in {path}");
                return ExitCode::FAILURE;
            }
        },
    };
    match rt.serve(&name, port).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("serve error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `tired schema <file>` — export the program's types/contracts as JSON Schema.
fn cmd_schema(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired schema` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let (program, diags) = tired_syntax::parse(&src);
    if diags.has_errors() {
        report(&diags, &src, path);
        return ExitCode::FAILURE;
    }
    let title = flag_value(args, "--title").unwrap_or_else(|| "TIRED types".into());
    match tired_runtime::schema::to_json_schema(&program, &title) {
        Some(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!("no `type` or `contract` declarations found in {path}");
            ExitCode::FAILURE
        }
    }
}

/// `tired replay <rec.json> <file>` — run a program offline against a recording.
async fn cmd_replay(args: &[String]) -> ExitCode {
    let (Some(rec), Some(path)) = (args.first(), args.get(1)) else {
        eprintln!("error: usage: tired replay <rec.json> <file.tired>");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let (compiled, diags) = compile(&src, path);
    if report(&diags, &src, path) {
        return ExitCode::FAILURE;
    }
    let Some(compiled) = compiled else {
        return ExitCode::FAILURE;
    };
    let mode = match RecordMode::replay_from(rec) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let rt = Runtime::with_mode(compiled, mode);
    match rt.run().await {
        Ok(Some(v)) if !matches!(v, Value::Null) => println!("=> {}", v.display()),
        Ok(_) => {}
        Err(e) => {
            eprintln!("runtime error: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn cmd_check(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired check` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let diags = tired_compiler::analyze(&src);
    let had_errors = report(&diags, &src, path);
    if had_errors {
        eprintln!("check failed: {} error(s)", diags.error_count());
        ExitCode::FAILURE
    } else {
        println!(
            "ok: {path} type-checks ({} warning(s))",
            diags.items().len()
        );
        ExitCode::SUCCESS
    }
}

fn cmd_fmt(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired fmt` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let (program, diags) = tired_syntax::parse(&src);
    if diags.has_errors() {
        report(&diags, &src, path);
        return ExitCode::FAILURE;
    }
    let formatted = tired_syntax::pretty::program(&program);
    if args.iter().any(|a| a == "--write") {
        if let Err(e) = std::fs::write(path, &formatted) {
            eprintln!("error: cannot write `{path}`: {e}");
            return ExitCode::FAILURE;
        }
        println!("formatted {path}");
    } else {
        print!("{formatted}");
    }
    ExitCode::SUCCESS
}

async fn cmd_test(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired test` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let (compiled, diags) = compile(&src, path);
    if report(&diags, &src, path) {
        return ExitCode::FAILURE;
    }
    let Some(compiled) = compiled else {
        return ExitCode::FAILURE;
    };
    let rt = Runtime::new(compiled);
    if rt.test_count() == 0 {
        println!("no tests found in {path}");
        return ExitCode::SUCCESS;
    }
    let report = rt.run_tests().await;
    for (desc, why) in &report.failures {
        println!("FAIL  {desc:?}\n      {why}");
    }
    println!(
        "\ntest result: {} — {} passed, {} failed (of {})",
        if report.ok() { "ok" } else { "FAILED" },
        report.passed,
        report.failures.len(),
        report.total()
    );
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_explain(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("error: `tired explain` needs a file");
        return ExitCode::FAILURE;
    };
    let Some(src) = read(path) else {
        return ExitCode::FAILURE;
    };
    let (compiled, diags) = compile(&src, path);
    if report(&diags, &src, path) {
        return ExitCode::FAILURE;
    }
    let Some(compiled) = compiled else {
        return ExitCode::FAILURE;
    };
    print!("{}", compiled.plan());
    ExitCode::SUCCESS
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).cloned()
}

/// Positional arguments after `--flow NAME` (everything that is not a known flag).
fn flow_args(args: &[String]) -> Vec<String> {
    let Some(i) = args.iter().position(|a| a == "--flow") else {
        return Vec::new();
    };
    args.iter()
        .skip(i + 2)
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect()
}
