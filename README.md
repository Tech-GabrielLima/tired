# TIRED — *The Internet Request & Execution Domain-language*

> **Languages:** **English** · [Português](README.pt-BR.md)

> **APIs are tired. So I built a language.**
>
> TIRED is a small **compiled domain-specific language for consuming HTTP APIs**, written from
> scratch in Rust. It is not a client library — it is a language with a lexer, a recursive-descent
> parser, a type checker, an SSA-style IR, an optimizer, and a concurrent runtime. The headline idea:
> the things you normally hand-roll around every API call — error handling, parallelism, retries,
> validation — become *properties of the language* that the compiler can check and the optimizer can
> exploit.

```tired
endpoint GitHub {
  base:    "https://api.github.com"
  auth:    Bearer($GITHUB_TOKEN)
  timeout: 5s
  retry:   3 backoff(exponential)
  cache:   ttl(5min)
}

type Repo { name: String  stargazers_count: Integer where (>= 0) }

flow Dashboard(username: String) -> User {
  fetch GitHub /users/{username} -> user: User       // runs first…

  fetch GitHub /users/{username}/repos               // …these two have no dependency
    | sort(by: .stargazers_count desc) | limit(3)     //    on each other, so the
    -> top: Repo[]                                     //    optimizer runs them in
  fetch GitHub /users/{username}/followers | limit(3) //    parallel automatically.
    -> followers

  log "{user.login}: {top.length} top repos, {followers.length} followers"
  return user
}
```

You wrote three sequential `fetch`es. TIRED's optimizer noticed the last two are independent and
scheduled them concurrently — no `Promise.all`, no `CompletableFuture`, no `asyncio.gather`.

---

## The three ideas that make it a language, not a library

### 1 · Network-dependent error handling — checked at compile time

A `fetch` annotated as `Result<T, E>` *cannot be used as if it succeeded*. Reading a field off it, or
forgetting to handle a failure case, is a **compile error** — there is no `NullPointerException` to
discover at 3am.

```text
$ tired check examples/broken.tired

error: no field `starz` on type `Repo`
  --> examples/broken.tired:15:25
   |
15 |   | filter(repo => repo.starz > 100)
   |                         ^^^^^
   = help: did you mean `stars`?

error: cannot read field `name` — `maybe` is a `Result<Repo, ?>`
  --> examples/broken.tired:22:11
   |
22 | log maybe.name
   |           ^^^^
   = help: `match` on it first and read the field inside the `Ok(...)` arm
   = note: the request might have failed; TIRED will not let you ignore that

error: unhandled error: `maybe` has type `Result<Repo, ?>` and may be an `Err`
  --> examples/broken.tired:19:32
   = help: `match maybe { ... }` and handle both `Ok` and `Err`, or `return maybe` to propagate it
```

A `match` on a `Result` must be **exhaustive**. A closed error union (`Result<T, NotFound | Unauthorized>`)
forces you to cover each variant; an open error type forces a catch-all `Err(e) => …`.

### 2 · Automatic parallel inference

The compiler lowers each body to an IR where data dependencies are explicit, then schedules the nodes
into **topological waves**. Independent requests land in the same wave and execute concurrently — you
never asked for it.

```text
$ tired explain examples/parallel.tired

main:
  wave 1:  ‖ 3 requests in parallel
    • fetch GitHub /users/torvalds -> a
    • fetch GitHub /users/octocat -> b
    • fetch GitHub /users/gvanrossum -> c
  wave 2:
    • log
```

### 3 · Dead-request elimination

A `fetch` whose result is never observed is **removed before anything hits the wire** — zero bytes
sent — and reported as a warning. (Found organically while building the benchmark: a `log` that didn't
reference the fetched values caused the optimizer to eliminate *every* request, which is exactly
correct.)

```text
warning: request `GitHub /users/torvalds/repos` is never used and was eliminated
   = note: dead-request elimination: 0 bytes were sent for it
```

---

## What's built vs. what's designed

This repository is the **working core** of the language — it compiles, type-checks, optimizes, and runs
real programs against real APIs. The original TIRED vision is a multi-year, multi-team product; the
parts below the line are deliberately **designed but not implemented**, and I'd rather say so than ship
hollow stubs.

| Built and tested ✅ | Designed, not implemented ⏳ |
|---|---|
| Lexer, parser, AST, `rustc`-style diagnostics (carets + "did you mean") | Python / Java FFI bindings (PyO3 / JNI over a C ABI) |
| Type system + checker: exhaustive `Result` handling, field typing, resolution | VS Code / IntelliJ extension packaging (the LSP server below already powers them) |
| IR + optimizer: **dead-request elimination**, **parallel inference** | WASM / native (LLVM) codegen, adaptive JIT |
| Concurrent runtime: wave scheduler, HTTP/2 via `reqwest`, retry/backoff, timeout, bearer auth, TTL cache, Prometheus-style counters | Distributed cluster mode, TiredHub registry |
| In-language **mock engine** + `test` blocks (offline, deterministic) | Redis-backed distributed cache |
| Runtime **contract** verification (`where` constraints) | OpenAPI / GraphQL schema *import*, `server` mode |
| **Language server** (`tired lsp`): live diagnostics, completion, hover | |
| **Time-travel** record & replay (`--record` / `tired replay`) | |
| **Schema inference** (`tired inspect` → typed `type`/`contract`) | |
| CLI: `run`, `check`, `test`, `explain`, `fmt`, `inspect`, `replay`, `lsp` | |

---

## Measured here

`cargo test --workspace` → **41 tests + 1 doc-test, 0 failures** across the five crates: lexer/parser,
type-checker (every flagship rule has both an accept and a reject test), optimizer (parallelism &
elimination), end-to-end runtime tests against an in-process HTTP server, schema inference, record/replay
round-trips, and the language server (diagnostics/completion/hover).

### Parallel-inference benchmark

```text
$ cargo test -p tired-runtime --test integration benchmark -- --nocapture

=== TIRED parallel-inference benchmark (6 fetches @ 100ms/hop) ===
  serial   (data-dependent chain): 620.1 ms
  parallel (independent, inferred): 104.7 ms
  speedup: 5.92x
```

> **Honesty note.** This measures the *engine* against an in-process server with a fixed per-request
> latency injected in software — it characterises how the scheduler overlaps requests, not a production
> network, and it is not a comparison against `httpx`/`reqwest`/`Feign` (I can't run those here). The
> serial figure is a genuine baseline: the same six fetches chained by a real data dependency, which
> forces one wave each. The point it proves is narrow and true — **sequentially-written, independent
> requests are executed concurrently with no user effort.**

---

## Architecture

```
  source.tired
      │
      ▼   ┌─────────────────────────── tired-syntax (zero deps) ───────────────────────────┐
  Lexer → Parser → AST  ·  spans  ·  rustc-style diagnostics  ·  pretty-printer (tired fmt)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────────────────── tired-compiler (zero deps) ─────────────────────────┐
  Type checker  →  IR lowering  →  Optimizer
   · exhaustive Result handling     · free-variable / dependency analysis
   · field typing + did-you-mean    · dead-request elimination
   · endpoint/variable resolution   · parallel inference (topological waves)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────── tired-runtime (tokio + reqwest, the only deps) ──────────────────┐
  Wave executor ── spawns each wave's fetches concurrently
      ├── HTTP engine: HTTP/2 pool, retry+backoff, timeout, bearer auth, TTL cache, metrics
      ├── Mock engine: offline, deterministic routing for `test`
      └── Contract verifier: runtime `where`-constraint checks
      ├── Record/replay: capture outcomes (`--record`) and serve them back (`replay`)
      └────────────────────────────────────────────────────────────────────────────────────┘
            ▲ tired-lsp — language server (reuses the compiler): diagnostics · completion · hover
            ▲ tired-cli — the `tired` binary: run · check · test · explain · fmt · inspect · replay · lsp
```

The split is deliberate: **the entire compiler front-end is dependency-free, std-only Rust.** Only the
runtime — the part that genuinely needs an async HTTP stack — pulls in `tokio` and `reqwest` (the LSP
reuses the compiler and only adds `serde_json`).

```
tired/
├── crates/
│   ├── tired-syntax/    lexer, parser, AST, diagnostics, pretty-printer  (no deps)
│   ├── tired-compiler/  types, checker, IR, optimizer                    (no deps)
│   ├── tired-runtime/   value model, eval, mock + HTTP engines, executor, contracts,
│   │                    schema inference (`inspect`), record/replay
│   ├── tired-lsp/       LSP server over stdio (diagnostics, completion, hover)
│   └── tired-cli/       the `tired` command-line driver
├── examples/            runnable .tired programs (live + offline)
└── docs/                DESIGN.md and the formal grammar (grammar.ebnf)
```

---

## Run it

```bash
cargo build                              # builds the `tired` binary
alias tired="cargo run -q -p tired-cli --"

# Offline (no network) — the mock engine + test blocks:
tired check   examples/broken.tired      # see the compiler reject bad code
tired test    examples/mocked.tired      # pipeline + contracts, all mocked
tired test    examples/error_handling.tired
tired explain examples/parallel.tired    # show the inferred parallel plan
tired fmt     examples/mocked.tired      # canonical formatting

# Live (uses the public GitHub API):
tired run examples/parallel.tired --show-plan --metrics
tired run examples/github_dashboard.tired --flow Dashboard octocat

# Schema inference — generate TIRED types from any JSON:
tired inspect https://api.github.com/users/octocat User

# Time-travel: record once (live), then replay forever (offline, deterministic):
tired run    examples/parallel.tired --record session.json
tired replay session.json examples/parallel.tired

# Language server (point your editor's LSP client at this):
tired lsp
```

Run the test suite and the benchmark:

```bash
cargo test --workspace
cargo test -p tired-runtime --test integration benchmark -- --nocapture
```

---

## A note on the name

`TIRED` is a backronym — *The Internet Request & Execution Domain-language* — and a small joke: every
other way to consume an API is a little tiring. The language can't fix the internet, but it can make
the compiler do the boring, error-prone parts for you.

---

## References

- The classic compiler pipeline (lexer → parser → typed AST → IR → optimizer) as in *Engineering a
  Compiler* (Cooper & Torczon) and Appel's *Modern Compiler Implementation*.
- Dependency-graph scheduling / topological levelling — the same idea behind build systems and dataflow
  schedulers, applied here to HTTP requests.
- `rustc`'s diagnostic style (primary span + caret + `help`/`note`) as the model for the error output;
  Levenshtein/optimal-string-alignment distance for "did you mean?".

---

*Code & comments in English. MIT licensed. A from-scratch language project — companion to the systems
portfolio (cudakit, nabla, nanollm) and the backends (ledger, matching-engine, raftkv).*
