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

### 4 · Request deduplication (CSE)

Two `fetch`es that issue the **identical** request (same endpoint, path, params, pipeline *and* the same
inputs) are collapsed — the later one reuses the first's result, so the same URL is never hit twice.
It's common-subexpression elimination, for the network. `tired explain` shows the second fetch rewritten
to a `let`:

```text
main:
  wave 1:
    • fetch GitHub /users/octocat -> a
  wave 2:
    • let -> b          # identical request — reuses `a`, 0 extra bytes
```

The pipeline itself is rich, too: `filter` · `map`/`pluck` · `sort` · `limit`/`take` · `skip` ·
`reverse` · `unique` · `flatten` · `count` · `sum`.

---

### 5 · It serves, too — `consume ↔ serve`

A `server { route GET /dashboard/{u} -> { … } }` turns TIRED into an API gateway / BFF.
Each route handler is ordinary TIRED code, so the **same optimizer parallelizes and
deduplicates its upstream calls** — you write a straight-line aggregation and get the
fastest safe gateway for free:

```text
$ tired explain examples/gateway.tired
server Gateway:
  route GET /dashboard/{..}:  [≤ 3 requests, up to 3 in parallel]
    wave 1:  ‖ 3 requests in parallel
      • fetch GitHub /users/{..} -> profile
      • fetch GitHub /users/{..}/repos -> top
      • fetch GitHub /users/{..}/followers -> followers
```

### 6 · It tells you the request cost — *before you ship*

That `[≤ 3 requests, up to 3 in parallel]` is **static request-cost analysis**: walking the
IR, the compiler bounds how many network calls any path through a route/flow can issue
(a `match` counts the max over its arms, a flow call adds that flow's cost). No HTTP
client tells you the blast radius of an endpoint at compile time — TIRED reads it off the
optimized IR.

---

## Why a language — and how it compares

Client libraries are excellent. The bet TIRED makes is that the *recurring, dangerous* parts of calling
an API — parallelism, error handling, retries, validation, testing — shouldn't be re-typed by hand in
every codebase. They should be **properties the compiler checks and the optimizer exploits**.

| | `requests`/`httpx` (Py) | `fetch`/`axios` (JS) | Feign/RestTemplate (Java) | **TIRED** |
|---|:---:|:---:|:---:|:---:|
| Independent calls run in parallel **automatically** | ✗ (manual `gather`) | ✗ (manual `Promise.all`) | ✗ | **✓** |
| **Won't compile** if you ignore a possible error | ✗ | ✗ | ✗ | **✓** |
| Identical requests **deduplicated**; unused ones **dropped** | ✗ | ✗ | ✗ | **✓** |
| Retry / backoff / timeout / cache as **declarative config** | manual | manual | annotations | **✓** |
| In-language **mocks** + tests (offline, deterministic) | separate libs | separate libs | partial | **✓** |
| **Record/replay** for deterministic offline runs | ✗ | ✗ | ✗ | **✓** |
| **Contract** validation of responses at runtime | ✗ | ✗ | ✗ | **✓** |
| Schema **inference** + **JSON Schema** export | ✗ | ✗ | ✗ | **✓** |
| One toolchain: type-check, `fmt`, LSP, explain-plan | n/a | n/a | n/a | **✓** |

Same task — fetch a user, fan out to two more calls, handle a 404 — in Python vs TIRED:

```python
# Python (httpx + asyncio): you wire the concurrency and remember to check the status.
async def dashboard(user):
    async with httpx.AsyncClient() as c:
        r = await c.get(f".../users/{user}")
        if r.status_code == 404:        # easy to forget; nothing forces it
            return None
        repos, followers = await asyncio.gather(    # manual parallelism
            c.get(f".../users/{user}/repos"),
            c.get(f".../users/{user}/followers"),
        )
        return build(r.json(), repos.json(), followers.json())
```

```tired
# TIRED: the 404 is a compile error if unhandled; the fan-out parallelizes itself.
flow Dashboard(user: String) -> User {
  fetch GitHub /users/{user}          -> u: Result<User, NotFound>
  fetch GitHub /users/{user}/repos     -> repos      # these two are independent,
  fetch GitHub /users/{user}/followers -> followers  # so they run concurrently
  match u { Ok(profile) => profile  Err(NotFound) => default_user() }
}
```

**Why use it:** the compiler refuses to let an error go unhandled, the optimizer turns your sequential
code into the fastest safe schedule (parallel where independent, deduped, dead calls removed), and one
toolchain gives you formatting, a language server, contract checks, mocks and record/replay — instead of
five libraries glued together.

---

## What's built vs. what's designed

This repository is the **working core** of the language — it compiles, type-checks, optimizes, and runs
real programs against real APIs. The original TIRED vision is a multi-year, multi-team product; the
parts below the line are deliberately **designed but not implemented**, and I'd rather say so than ship
hollow stubs.

| Built and tested ✅ | Designed, not implemented ⏳ |
|---|---|
| Lexer, parser, AST, `rustc`-style diagnostics (carets + "did you mean") | Java (JNI) bindings |
| Type system + checker: exhaustive `Result` handling, field typing, resolution | IntelliJ plugin (the VS Code extension is built) |
| IR + optimizer: **dead-request elimination**, **parallel inference**, **request deduplication** | WASM / native (LLVM) codegen, adaptive JIT |
| **Static request-cost analysis** (max requests & parallelism per route/flow) | Distributed cluster mode, TiredHub registry |
| Concurrent runtime: wave scheduler, HTTP/2, retry/backoff, timeout, bearer auth, TTL cache, metrics | Redis-backed distributed cache |
| **Full HTTP verbs** (GET/POST/PUT/PATCH/DELETE) + JSON bodies; mutations never reordered/deduped/auto-retried | OpenAPI / GraphQL schema *import* |
| **`server` mode** — serve HTTP routes whose handlers consume APIs (auto-parallelized) | |
| In-language **mock engine** + `test` blocks; runtime **contract** verification | |
| **Language server** (`tired lsp`) + **VS Code extension**; **Python bindings** (PyO3, pip) | |
| **Time-travel** record & replay; **schema inference** + **JSON Schema export** | |
| CLI: `run`, `check`, `test`, `explain`, `fmt`, `inspect`, `schema`, `serve`, `replay`, `lsp` | |

---

## Measured here

`cargo test --workspace` → **51 tests + 1 doc-test, 0 failures** across six crates: lexer/parser,
type-checker (every flagship rule has both an accept and a reject test), optimizer (parallelism,
dead-request elimination, deduplication & request-cost), end-to-end runtime tests against an in-process
HTTP server — including an **end-to-end `server`-mode test** that starts a TIRED gateway and asserts it
aggregates two upstreams in parallel — schema inference + JSON Schema export, record/replay round-trips,
and the language server. The Python bindings (PyO3) build into an `abi3` module and are exercised from
Python.

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
      └── HTTP server (`serve`): route handlers run through the same optimizer
      └────────────────────────────────────────────────────────────────────────────────────┘
            ▲ tired-lsp — language server (reuses the compiler): diagnostics · completion · hover
            ▲ tired-py  — Python bindings (PyO3, abi3): check · run · inspect · schema
            ▲ tired-cli — `tired`: run · check · test · explain · fmt · inspect · schema · serve · replay · lsp
```

The split is deliberate: **the entire compiler front-end is dependency-free, std-only Rust.** Only the
runtime — the part that genuinely needs an async HTTP stack — pulls in `tokio` and `reqwest` (the LSP
reuses the compiler and only adds `serde_json`).

```
tired/
├── crates/
│   ├── tired-syntax/    lexer, parser, AST, diagnostics, pretty-printer  (no deps)
│   ├── tired-compiler/  types, checker, IR, optimizer, request-cost      (no deps)
│   ├── tired-runtime/   eval, mock + HTTP engines, wave executor, contracts,
│   │                    schema inference, record/replay, HTTP server
│   ├── tired-lsp/       LSP server over stdio (diagnostics, completion, hover)
│   ├── tired-py/        Python bindings (PyO3 / maturin)
│   └── tired-cli/       the `tired` command-line driver
├── editors/vscode/      VS Code extension (grammar + LSP client)
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

# server mode — TIRED as an API gateway (handlers auto-parallelize their upstreams):
tired explain examples/gateway.tired     # plan + request cost, no network
tired serve   examples/gateway.tired     # serve it on http://127.0.0.1:8088/api/...

# Language server (point your editor's LSP client at this):
tired lsp
```

From **Python** (PyO3 bindings):

```bash
pip install maturin && (cd crates/tired-py && maturin develop)
python -c "import tired; print(tired.inspect('{\"id\":1}', 'User'))"
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
