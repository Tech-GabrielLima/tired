# hale — *HTTP API Language & Engine*

> **Languages:** **English** · [Português](README.pt-BR.md)
>
> **Docs:** [Language Reference](docs/LANGUAGE.md) · [Design & internals](docs/DESIGN.md) · [Grammar (EBNF)](docs/grammar.ebnf)

> ***hale*** *(adj.)* — strong and healthy. Consuming an API should leave your code that way.
>
> hale is a small **compiled domain-specific language for consuming (and serving) HTTP APIs**, written
> from scratch in Rust. It is not a client library — it is a language with a lexer, a recursive-descent
> parser, a type checker, an SSA-style IR, an optimizer, and a concurrent runtime. The headline idea:
> the things you normally hand-roll around every API call — error handling, parallelism, retries,
> validation, *and even your latency/cost budget* — become *properties of the language* that the
> compiler can check and the optimizer can exploit.

```hale
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

You wrote three sequential `fetch`es. hale's optimizer noticed the last two are independent and
scheduled them concurrently — no `Promise.all`, no `CompletableFuture`, no `asyncio.gather`.

---

## The three ideas that make it a language, not a library

### 1 · Network-dependent error handling — checked at compile time

A `fetch` annotated as `Result<T, E>` *cannot be used as if it succeeded*. Reading a field off it, or
forgetting to handle a failure case, is a **compile error** — there is no `NullPointerException` to
discover at 3am.

```text
$ hale check examples/broken.hale

error: no field `starz` on type `Repo`
  --> examples/broken.hale:15:25
   |
15 |   | filter(repo => repo.starz > 100)
   |                         ^^^^^
   = help: did you mean `stars`?

error: cannot read field `name` — `maybe` is a `Result<Repo, ?>`
  --> examples/broken.hale:22:11
   |
22 | log maybe.name
   |           ^^^^
   = help: `match` on it first and read the field inside the `Ok(...)` arm
   = note: the request might have failed; hale will not let you ignore that

error: unhandled error: `maybe` has type `Result<Repo, ?>` and may be an `Err`
  --> examples/broken.hale:19:32
   = help: `match maybe { ... }` and handle both `Ok` and `Err`, or `return maybe` to propagate it
```

A `match` on a `Result` must be **exhaustive**. A closed error union (`Result<T, NotFound | Unauthorized>`)
forces you to cover each variant; an open error type forces a catch-all `Err(e) => …`.

### 2 · Automatic parallel inference

The compiler lowers each body to an IR where data dependencies are explicit, then schedules the nodes
into **topological waves**. Independent requests land in the same wave and execute concurrently — you
never asked for it.

```text
$ hale explain examples/parallel.hale

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
It's common-subexpression elimination, for the network. `hale explain` shows the second fetch rewritten
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

### 4½ · Request fusion / batching — vectorization for the wire 🧬

If an endpoint declares a `batch:` rule, the optimizer goes further than dedup: it collapses
several GETs that differ **only in the last path segment** into a *single* batched call, then
**scatters** the array result back to each binding by a join key. N round-trips become one.

```hale
endpoint GH { base: "..."  batch: param("ids") key(.id) }

fetch GH /users/1 -> a   //   the optimizer fuses these three
fetch GH /users/2 -> b   //   into ONE  GET /users?ids=1,2,3
fetch GH /users/3 -> c   //   and scatters the array back by .id
```

```text
$ hale explain examples/batch.hale
  wave 1:
    • fetch GH /users?ids=… [batched ×3] -> __batch_0
  wave 2:
    • scatter __batch_0.id (from batch) -> a
    • scatter __batch_0.id (from batch) -> b
    • scatter __batch_0.id (from batch) -> c
```

The request-cost analysis then reports `≤ 1 request` instead of 3 — the network is hit once.
No client library does this automatically; it's loop fusion, for HTTP.

---

### 5 · It serves, too — `consume ↔ serve`

A `server { route GET /dashboard/{u} -> { … } }` turns hale into an API gateway / BFF.
Each route handler is ordinary hale code, so the **same optimizer parallelizes and
deduplicates its upstream calls** — you write a straight-line aggregation and get the
fastest safe gateway for free:

```text
$ hale explain examples/gateway.hale
server Gateway:
  route GET /dashboard/{..}:  [≤ 3 requests, up to 3 in parallel]
    wave 1:  ‖ 3 requests in parallel
      • fetch GitHub /users/{..} -> profile
      • fetch GitHub /users/{..}/repos -> top
      • fetch GitHub /users/{..}/followers -> followers
```

### 6 · It tells you the request cost — *before you ship*

That `[≤ 3 requests, up to 3 in parallel, 1 hop deep]` is **static request-cost analysis**:
walking the IR, the compiler bounds how many network calls any path through a route/flow can
issue, how many run **concurrently**, and the **critical-path depth** — the number of
*sequential* round-trips, which is what actually dominates latency (a `match` counts the max
over its arms, a flow call adds that flow's cost). No HTTP client tells you the blast radius of
an endpoint at compile time — hale reads it off the optimized IR.

### 7 · A compile-time SLA — `budget(...)` 🚀

Because the cost is known statically, you can **assert** it. Annotate a flow or route with a
`budget` — over requests, fan-out, critical-path hops, **or wall-clock `p99`** — and the
compiler refuses to build if any path can exceed it:

```hale
endpoint Billing { base: "..."  latency: 120ms }     // declared per-hop latency

flow Overview(id: String) -> Customer budget(requests: 3, parallel: 2, hops: 2, p99: 400ms) {
  fetch Billing /customers/{id} -> customer: Customer
  fetch Billing /customers/{customer.id}/invoices      | count() -> invoices
  fetch Billing /customers/{customer.id}/subscriptions | count() -> subs
  return customer
}
```

```text
$ hale explain examples/sla.hale
flow Overview(id):  [≤ 3 requests, up to 2 in parallel, 2 hops deep, ~240ms critical path]
                    (budget: requests ≤ 3, parallel ≤ 2, hops ≤ 2, p99 ≤ 400ms)
  effects: reads {Billing}
```

`p99` is summed over the critical path from each endpoint's declared `latency:`; promise a
latency you can't prove (an undeclared hop) and the compiler says so. A performance budget that
**lives in the type system**, not a dashboard you read after the incident. `hale explain` also
prints a proved **effect signature** (`reads {Billing}` / `reads+writes {…}`) — capability
information for every flow.

### 7½ · N+1 query detection — the #1 client perf bug, caught at compile time 🪤

Fetch a list, then loop and fetch one thing per element — the **N+1 query**. hale finds it with
a real **data-flow analysis**: it tracks which values came from the network (the *1*) and which
derive from a `for` element (the *N*), threading that provenance through `let`s, `match` arms,
and nested loops.

```hale
fetch GH /users -> users: User[]
for u in users {
  fetch GH /users/{u.id}/repos -> repos   // ← one request per user
  log "{u.login}: {repos.length}"
}
```

```text
$ hale check examples/nplus1.hale
warning: N+1 query: `GH /users/{u.id}/repos` runs once per element of `users`
   = note: the classic 1+N: the collection was itself fetched, then one request fires per element
$ hale explain examples/nplus1.hale
flow Dashboard():  [unbounded requests — a fetch runs once per `for` element (N+1); …]
```

It sees through an indirection (`let id = u.id; fetch …/{id}`), reports **nested loops** as
`Nˆ2`, and separately flags a **loop-invariant** read ("hoist it out"). Two tiers: the lint is a
*warning* (an N+1 over a tiny list may be fine), but a per-element fetch makes the [cost](#6--it-tells-you-the-request-cost--before-you-ship)
**unbounded**, so any `budget(...)` on that flow becomes a hard **compile error** — you can't
promise a bound a loop can blow.

And when the endpoint supports it, the compiler doesn't just complain — it **fixes it**. See §7¾.

### 7¾ · …and then it fixes it — automatic loop fusion 🔧

If the per-element endpoint declares a `batch:` rule, the optimizer **rewrites the loop**: it
hoists the fetch out, gathers every key into one batched call, and replaces the in-loop fetch
with a pure *scatter*. `1 + N` round-trips become `1 + 1` — loop-invariant code motion + batching,
for the network.

```hale
endpoint GH { base: "..."  batch: param("ids") key(.id) }
fetch GH /users -> users: User[]
for u in users {
  fetch GH /users/{u.id} -> detail   // ← auto-fused, not warned: the compiler hoists it
  log "{detail.login}"
}
```

```text
$ hale explain examples/loop_fusion.hale
flow Logins():  [≤ 2 requests, up to 1 in parallel, 2 hops deep]   ← was unbounded (N+1)
  wave 1:  • fetch GH /users -> users
  wave 2:  • fetch GH /users?ids=… [batched ×N, fused from loop] -> __loopbatch_0
  wave 3:  • for u in … (per-element loop)
             wave 1:  • scatter __loopbatch_0.id (from batch) -> detail
```

A runtime test confirms the loop's three per-element GETs hit the network **once**. (Multi-level
fusion of nested loops follows the same mechanism, per innermost level.)

### 8 · Information-flow control — data governance in the compiler 🔒

Label a field `PII` or `Secret` and the compiler **tracks it through the program** along a
lattice `Public < PII < Secret`. Each endpoint has a `clearance:` (the most sensitive data it
may receive); a `log` or HTTP response is cleared only for `Public`. Data may never flow to a
sink below its label:

```hale
type Customer { id: Integer  email: PII  card: Secret }
endpoint Analytics { base: "..."  clearance: Public }   // refuses PII/Secret
```

```text
$ hale check governance.hale
error: `PII`-labelled value `email` must not flow to the request to `Analytics` (cleared for `Public`)
   = note: information-flow control: a value's label may not exceed the sink's clearance
```

A secret may still flow *into* an endpoint that needs it (undeclared clearance = top), and the
check is transitive — returning a whole record that merely *contains* a `Secret`/`PII` field is
rejected too. This is GDPR / data-residency / secret-leak prevention enforced **at compile
time** — the same `secret_field_of` taint machine, now a real Denning information-flow lattice.

### 9 · The compiler knows which writes are safe to retry — `idempotent` ♻️

A mutation is never silently re-sent. But mark it `idempotent(key: …)` and hale *proves* the
write is safe to repeat — re-enabling retry for it and attaching an `Idempotency-Key` header
(the Stripe model). No library can say *"this `POST` is retry-safe and that one isn't."* hale
can, because it's in the type.

```hale
fetch POST Billing /charges idempotent(key: order.id) body { ... } -> r
```

---

## Why a language — and how it compares

Client libraries are excellent. The bet hale makes is that the *recurring, dangerous* parts of calling
an API — parallelism, error handling, retries, validation, testing — shouldn't be re-typed by hand in
every codebase. They should be **properties the compiler checks and the optimizer exploits**.

| | `requests`/`httpx` (Py) | `fetch`/`axios` (JS) | Feign/RestTemplate (Java) | **hale** |
|---|:---:|:---:|:---:|:---:|
| Independent calls run in parallel **automatically** | ✗ (manual `gather`) | ✗ (manual `Promise.all`) | ✗ | **✓** |
| **Won't compile** if you ignore a possible error | ✗ | ✗ | ✗ | **✓** |
| Identical requests **deduplicated**; unused ones **dropped** | ✗ | ✗ | ✗ | **✓** |
| Near-identical GETs **fused into one batched call** automatically | ✗ | ✗ | ✗ | **✓** |
| Retry / backoff / timeout / cache as **declarative config** | manual | manual | annotations | **✓** |
| In-language **mocks** + tests (offline, deterministic) | separate libs | separate libs | partial | **✓** |
| **Record/replay** for deterministic offline runs | ✗ | ✗ | ✗ | **✓** |
| **Contract** validation of responses at runtime | ✗ | ✗ | ✗ | **✓** |
| Schema **inference** + **JSON Schema** export | ✗ | ✗ | ✗ | **✓** |
| **Request / latency (`p99`) budget** enforced *at compile time* | ✗ | ✗ | ✗ | **✓** |
| **N+1 detection + automatic loop fusion** — a per-element fetch is flagged *and* batched | ✗ | ✗ | ✗ | **✓** |
| **Information-flow control**: PII/Secret can't reach a lower-clearance sink | ✗ | ✗ | ✗ | **✓** |
| **Retry-safety in the type**: proves which writes are `idempotent` | ✗ | ✗ | ✗ | **✓** |
| One toolchain: type-check, `fmt`, LSP, explain-plan | n/a | n/a | n/a | **✓** |

Same task — fetch a user, fan out to two more calls, handle a 404 — in Python vs hale:

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

```hale
# hale: the 404 is a compile error if unhandled; the fan-out parallelizes itself.
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
real programs against real APIs. The original hale vision is a multi-year, multi-team product; the
parts below the line are deliberately **designed but not implemented**, and I'd rather say so than ship
hollow stubs.

| Built and tested ✅ | Designed, not implemented ⏳ |
|---|---|
| Lexer, parser, AST, `rustc`-style diagnostics (carets + "did you mean") | OpenAPI / GraphQL *import* → endpoints+types; OpenAPI/SDK *export* |
| Type system + checker: exhaustive `Result` handling, field typing, resolution | Profile-guided (record/replay) adaptive scheduling; pagination primitive |
| IR + optimizer: **dead-request elimination**, **parallel inference**, **request deduplication** | Distributed wave execution; haleHub registry; Redis cache |
| **Auto request fusion / batching** (`/u/1`,`/u/2`→`/u?ids=1,2`, scatter by key) | Property-based fuzzing from contracts; freshness types |
| **N+1 query detection** (data-flow) **+ automatic loop fusion** (`for` fetch → 1 batched call) | Java (JNI) bindings; IntelliJ plugin; WASM/LLVM codegen, JIT |
| **Static cost analysis** (max requests, parallelism, critical-path hops & `p99` latency) | |
| **Compile-time SLA**: `budget(requests/parallel/hops/p99)` enforced against the analysis | |
| **Information-flow control**: PII/Secret label lattice + per-endpoint `clearance` | |
| **Idempotency / retry-safety in the type** (`idempotent(key:)`); **effect signatures** | |
| Concurrent runtime: wave scheduler, HTTP/2, retry/backoff, timeout, bearer auth, TTL cache, metrics | |
| **Full HTTP verbs** (GET/POST/PUT/PATCH/DELETE) + JSON bodies; mutation safety | |
| **`server` mode** — serve HTTP routes whose handlers consume APIs (auto-parallelized) | |
| In-language **mock engine** + `test` blocks; runtime **contract** verification | |
| **Language server** (`hale lsp`) + **VS Code extension**; **Python bindings** (PyO3, pip) | |
| **Time-travel** record & replay; **schema inference** + **JSON Schema export** | |
| CLI: `run`, `check`, `test`, `explain`, `fmt`, `inspect`, `schema`, `serve`, `replay`, `lsp` | |

---

## Measured here

`cargo test --workspace` → **87 tests + 1 doc-test, 0 failures** across six crates: lexer/parser,
type-checker (every flagship rule — exhaustive `Result`, **information-flow control**, **budget / `p99`
enforcement**, **idempotency** — has both an accept and a reject test), optimizer (parallelism,
dead-request elimination, deduplication, **request fusion**, **automatic loop fusion**, request-cost,
critical-path hops & latency), **N+1 detection** (1+N over a fetched list, detection through an
intermediate `let`, nested-loop `Nˆ2`, loop-invariant reads, the budget→unbounded rejection, and
suppression once a loop is auto-fused), end-to-end runtime tests against an in-process HTTP server —
including a **request-fusion test** that asserts 3 GETs hit the network **once**, a **loop-fusion test**
that asserts a per-element `for` loop collapses to a single batched call, a **`for`-loop test** that
asserts the body fans out to one request per element, and an **end-to-end `server`-mode test** that starts
a hale gateway and asserts it aggregates two upstreams in parallel — schema inference + JSON Schema export,
record/replay round-trips, and the language server. The Python bindings (PyO3) build into an `abi3` module
and are exercised from Python.

### Parallel-inference benchmark

```text
$ cargo test -p hale-runtime --test integration benchmark -- --nocapture

=== hale parallel-inference benchmark (6 fetches @ 100ms/hop) ===
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
  source.hale
      │
      ▼   ┌─────────────────────────── hale-syntax (zero deps) ───────────────────────────┐
  Lexer → Parser → AST  ·  spans  ·  rustc-style diagnostics  ·  pretty-printer (hale fmt)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────────────────── hale-compiler (zero deps) ─────────────────────────┐
  Type checker  →  IR lowering  →  Optimizer
   · exhaustive Result handling     · free-variable / dependency analysis
   · field typing + did-you-mean    · dead-request elimination
   · endpoint/variable resolution   · parallel inference (topological waves)
      │   └────────────────────────────────────────────────────────────────────────────────┘
      ▼   ┌────────────── hale-runtime (tokio + reqwest, the only deps) ──────────────────┐
  Wave executor ── spawns each wave's fetches concurrently
      ├── HTTP engine: HTTP/2 pool, retry+backoff, timeout, bearer auth, TTL cache, metrics
      ├── Mock engine: offline, deterministic routing for `test`
      └── Contract verifier: runtime `where`-constraint checks
      ├── Record/replay: capture outcomes (`--record`) and serve them back (`replay`)
      └── HTTP server (`serve`): route handlers run through the same optimizer
      └────────────────────────────────────────────────────────────────────────────────────┘
            ▲ hale-lsp — language server (reuses the compiler): diagnostics · completion · hover
            ▲ hale-py  — Python bindings (PyO3, abi3): check · run · inspect · schema
            ▲ hale-cli — `hale`: run · check · test · explain · fmt · inspect · schema · serve · replay · lsp
```

The split is deliberate: **the entire compiler front-end is dependency-free, std-only Rust.** Only the
runtime — the part that genuinely needs an async HTTP stack — pulls in `tokio` and `reqwest` (the LSP
reuses the compiler and only adds `serde_json`).

```
hale/
├── crates/
│   ├── hale-syntax/    lexer, parser, AST, diagnostics, pretty-printer  (no deps)
│   ├── hale-compiler/  types, checker, IR, optimizer, request-cost      (no deps)
│   ├── hale-runtime/   eval, mock + HTTP engines, wave executor, contracts,
│   │                    schema inference, record/replay, HTTP server
│   ├── hale-lsp/       LSP server over stdio (diagnostics, completion, hover)
│   ├── hale-py/        Python bindings (PyO3 / maturin)
│   └── hale-cli/       the `hale` command-line driver
├── editors/vscode/      VS Code extension (grammar + LSP client)
├── examples/            runnable .hale programs (live + offline)
└── docs/                DESIGN.md and the formal grammar (grammar.ebnf)
```

---

## Run it

```bash
cargo build                              # builds the `hale` binary
alias hale="cargo run -q -p hale-cli --"

# Offline (no network) — the mock engine + test blocks:
hale check   examples/broken.hale      # see the compiler reject bad code
hale test    examples/mocked.hale      # pipeline + contracts, all mocked
hale test    examples/error_handling.hale
hale explain examples/parallel.hale    # show the inferred parallel plan
hale fmt     examples/mocked.hale      # canonical formatting

# Live (uses the public GitHub API):
hale run examples/parallel.hale --show-plan --metrics
hale run examples/github_dashboard.hale --flow Dashboard octocat

# Schema inference — generate hale types from any JSON:
hale inspect https://api.github.com/users/octocat User

# Time-travel: record once (live), then replay forever (offline, deterministic):
hale run    examples/parallel.hale --record session.json
hale replay session.json examples/parallel.hale

# server mode — hale as an API gateway (handlers auto-parallelize their upstreams):
hale explain examples/gateway.hale     # plan + request cost, no network
hale serve   examples/gateway.hale     # serve it on http://127.0.0.1:8088/api/...

# Compile-time SLA (requests/hops/p99) + information-flow control + idempotency:
hale explain examples/sla.hale         # cost incl. critical-path hops + latency + budget
hale check   examples/governance.hale  # PII/Secret clearance + idempotent writes

# Language server (point your editor's LSP client at this):
hale lsp
```

From **Python** (PyO3 bindings):

```bash
pip install maturin && (cd crates/hale-py && maturin develop)
python -c "import hale; print(hale.inspect('{\"id\":1}', 'User'))"
```

Run the test suite and the benchmark:

```bash
cargo test --workspace
cargo test -p hale-runtime --test integration benchmark -- --nocapture
```

---

## A note on the name

***hale*** *(adjective)* — free from defect, disease, or infirmity; sound, robust, *hale and hearty*.
It doubles as a backronym, **H**TTP **A**PI **L**anguage & **E**ngine. The bet of the project is in the
word: consuming an API is usually a little exhausting — manual concurrency, forgotten error checks,
mystery latency, leaked tokens. hale moves that work into the compiler so what you ship comes out
*robust by construction*.

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
