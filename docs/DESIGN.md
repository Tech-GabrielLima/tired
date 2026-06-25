# TIRED — Design

This document explains how the implementation works, stage by stage, and is honest about where the
lines are drawn. The guiding principle throughout the front-end is **no false positives**: a check only
fires when the type information needed to justify it is actually present.

---

## 1. Pipeline overview

```
source ──▶ lex ──▶ parse ──▶ check ──▶ lower ──▶ optimize ──▶ execute
            │        │         │         │          │            │
          tokens    AST     diagnostics  IR    waves + DRE   values / I/O
```

The first three stages (`tired-syntax`) and the next three (`tired-compiler`) are **zero-dependency,
std-only Rust**. Only execution (`tired-runtime`) uses third-party crates (`tokio`, `reqwest`,
`serde_json`).

---

## 2. Lexer (`tired-syntax/lexer.rs`)

A hand-written scanner producing a flat `Vec<Token>` terminated by `Eof`. Notable rules:

- **Duration literals** `5s`, `300ms`, `5min`, `2h` are recognised by reading the digits, then a
  trailing alphabetic run; if that run is a known unit it becomes a `Duration` (normalised to ms),
  otherwise the integer and the identifier are emitted separately.
- **`..` disambiguation**: a `.` is a decimal point only if followed by a digit, so `1..100` lexes as
  `Int(1) DotDot Int(100)`, never `1.` then `.100`.
- **`$NAME`** is an environment-variable token.
- **Strings** keep their *raw* inner text; interpolation is split later by the parser, which means the
  lexer needs no expression context.

Every token carries a byte-range `Span`, threaded through every later stage.

## 3. Parser (`tired-syntax/parser.rs`)

Recursive descent with a Pratt expression sub-parser for binary-operator precedence. Two details worth
calling out:

- **Struct-literal ambiguity.** `match scrut { … }` would parse `scrut {` as a record literal. A
  `no_record` flag (à la Rust) suppresses record detection while parsing a `match` scrutinee, and is
  reset inside `(...)`/`[...]`.
- **Contextual keywords.** Words like `retry` are keywords inside a `match` arm but ordinary names as an
  endpoint setting key. A "lenient name" helper accepts a keyword token as an identifier in the
  positions where only a name is grammatical.

On error the parser records a diagnostic and recovers to the next top-level item, so one typo doesn't
cascade.

## 4. Type system & checker (`tired-compiler/types.rs`, `check.rs`)

Types: `Int`, `Float`, `Bool`, `String`, `Null`, `Duration`, semantic scalars (`Url`, `Email`, …),
`Record(name)`, `Array`, `Optional`, and `Result<T, ErrDomain>` where the error domain is either
**open** (a single named error type → needs a catch-all) or a **closed set** of variants (a
`A | B` union → each must be handled). Inference is shallow: known annotations and declared records give
types; everything else is `Unknown`, which suppresses checks.

Three families of checks:

1. **Resolution** — `fetch` endpoints must be declared; an unknown one yields a "did you mean?" over the
   declared endpoints. Unknown lowercase identifiers used as a field receiver or a path parameter are
   reported similarly. (Upper-case names are treated as types/constructors and never flagged.)
2. **Field typing** — `x.field` on a known `Record` is validated against the record's fields; pipeline
   predicates (`filter(r => r.field)`, `sort(by: .field)`) are checked against the *element* type
   derived from the binding annotation. Wrong fields get a Levenshtein/optimal-string-alignment
   suggestion.
3. **Network-dependent error handling** (the flagship) —
   - a `Result`-typed binding must be `match`ed or `return`ed (propagated); otherwise it's an
     `unhandled error`;
   - reading a field off a `Result` is rejected ("match it first");
   - a `match` on a `Result` must be **exhaustive** over `Ok` and the error domain.

## 5. IR & lowering (`tired-compiler/ir.rs`, `lower.rs`)

Each body (the top-level script, a flow, a `match`-arm block) lowers to a `Body`: a flat `Vec<Node>`.
Expressions stay as their AST form — only *statements* become nodes. The key product of lowering is the
**dependency graph**:

- For each node we compute the set of free variables it **reads** (a structural walk that excludes
  lambda parameters and pattern bindings).
- A node depends on the most recent earlier node that **wrote** each variable it reads.
- **Effect** nodes (`log`, `return`, `assert`, flow calls, `match`) additionally chain to the previous
  effect, so observable order (e.g. log ordering) is preserved while pure computations stay free to
  reorder.

## 6. Optimizer (`tired-compiler/optimize.rs`)

Three passes over every body (recursing into `match` arms), in this order:

- **Request deduplication (CSE).** Each fetch gets a *signature* — endpoint + path + params + pipeline
  (rendered via the pretty-printer, so it is span-insensitive) plus the producers of its inputs (its read
  set and dependency ids). Two fetches with equal signatures issue the identical request with the
  identical inputs, so the later one is rewritten into `let b = a` (reusing the first binding). Running
  before liveness means the rewrite keeps the first fetch alive. The result: the network is hit once.
- **Dead-request elimination.** Liveness is backward reachability from the observable (effect) nodes.
  Anything unreached is dead; a dead *fetch* is reported (zero bytes will be sent) and excluded from the
  schedule.
- **Parallel inference.** Live nodes are levelled topologically: `level(n) = 1 + max(level of live
  deps)`. Nodes sharing a level form a **wave**. Because dependency edges always point to earlier ids, a
  single forward pass computes all levels. The waves are exactly the concurrency plan the runtime
  executes, and what `tired explain` prints.

## 7. Runtime (`tired-runtime`)

### Executor (`exec.rs`)
`run_body` walks the waves in order. For each wave it **spawns every fetch concurrently** on `tokio`
(each task gets an `Arc` to the shared state and an owned snapshot of the environment), awaits them, then
runs the wave's non-fetch nodes in program order. Because nodes in a wave are mutually independent, a
pre-wave environment snapshot is sufficient and there is no shared mutation during concurrency.

Control flow is handled here because it does I/O: a statement that calls a declared `flow` runs that flow
(recursively, boxed for async recursion); a `match` evaluates its scrutinee, picks an arm, and — for a
`retry` arm — runs the arm's effects (`wait`, side-effecting calls) and **re-runs the fetch that
produced the scrutinee**, capped at a fixed number of attempts.

### Backends
- **HTTP** (`http.rs`): a pooled `reqwest` client (HTTP/2 over rustls). Per request it applies the
  endpoint's timeout, bearer/API-key auth, and a TTL response cache, and retries `429`/`5xx`/network
  errors with constant or exponential backoff. Atomic counters expose request/cache-hit/retry/error
  totals (`--metrics`).
- **Mock** (`mock.rs`): a `mock` block becomes a routing table; a request is matched by method + path,
  parameters are captured and exposed to the response body as `$name`, and a response naming an error
  variant (`NotFound`, `RateLimit(ms)`, …) becomes a typed failure. This makes `tired test` fully
  offline and deterministic.
- **Contracts** (`contracts.rs`): when a fetch binding's record type carries `where (...)` constraints,
  the response is validated. Only declared constraints bite — extra/unknown fields are tolerated — so
  contracts catch real violations without being brittle against API evolution.

### Values (`value.rs`)
JSON plus durations and `Ok`/`Err`. A `Result`-annotated fetch wraps its outcome as `Ok(body)` /
`Err(variant)`; a plain fetch unwraps success and promotes a failure to a runtime error (with a hint to
annotate it as `Result<...>`).

---

## 8. Tooling

Three pieces of tooling reuse the core rather than re-implementing it.

- **Schema inference (`infer.rs`, `tired inspect`).** Given a JSON sample (a live URL or a file), it
  reconstructs TIRED `type` declarations: objects become typed records, arrays of objects become `Elem[]`
  with a *merged* element type (a field present in only some elements is marked nullable), and strings get
  semantic types (`Url`, `Email`, `DateTime`, `UUID`) by light heuristics. Pure and unit-tested.
- **Record & replay (`record.rs`, `--record` / `tired replay`).** In record mode every request's raw
  outcome is captured under a canonical key (`GET endpoint/path?sortedquery`) and written as JSON. In
  replay mode that file is served back *before* the network is touched — a missing key is a hard error, so
  a replay is fully deterministic and offline. This is "time-travel" debugging: reproduce exactly what an
  API returned without the live service.
- **Language server (`tired-lsp`, `tired lsp`).** A stdio LSP that runs `tired_compiler::analyze` on every
  edit and publishes the same diagnostics the CLI prints (byte spans mapped to UTF-16 LSP ranges), plus
  keyword/endpoint **completion** and **hover**. The message handler is pure and unit-tested; the loop only
  adds Content-Length framing. It depends only on the compiler + `serde_json`. A thin **VS Code extension**
  (`editors/vscode`) packages it together with a TextMate grammar.
- **JSON Schema export (`schema.rs`, `tired schema`).** Emits a JSON Schema (2020-12) for the declared
  `type`/`contract`s — field types map to JSON Schema types (with `format`s for `Url`/`Email`/…) and
  `where` constraints become `minimum`/`maxLength`/… keywords.
- **`server` mode (`server.rs`, `tired serve`).** Closes the loop: a `server { route ... }` is served by a
  hand-rolled tokio HTTP/1.1 server. Each request binds its path params (plus `query`/`body`) and runs the
  route's handler **through the same executor** — so a fan-out aggregation in a handler is parallelized and
  deduplicated for free. An API gateway whose concurrency the compiler writes.
- **Static request-cost analysis (`cost.rs`).** Walking the optimized IR, it bounds the number of network
  requests any path through a flow/route can issue and how many run in parallel (a `match` takes the *max*
  over arms; a flow call adds that flow's cost; recursion is broken). Surfaced by `tired explain`.
- **Python bindings (`tired-py`, PyO3 abi3).** The compiler + runtime exposed to Python (`check`, `run`,
  `inspect`, `json_schema`, `explain`) as a single `abi3` wheel that works on CPython 3.8+.

### Mutation safety

Adding non-GET verbs forced a correctness decision the optimizer now encodes: a **mutation is an effect**.
A non-GET fetch is marked as an effect node, which means it is never reordered, never deduplicated, never
eliminated when its result is unused, and never auto-retried on failure (the request may already have been
received). GETs remain freely parallelizable, cacheable, dedupable and retryable. The compiler knows the
difference between a safe read and a side effect.

---

## 9. Deliberate limitations

These keep the implementation honest and focused on the language ideas:

- **Type-checker inference is annotation-driven.** Without a binding annotation a fetch result is
  `Unknown`, so field and exhaustiveness checks don't apply to it. Opting into `Result<...>` is how you opt
  into checked error handling. (`tired inspect` generates types offline from a sample, but the type checker
  itself does not infer response shapes from the network.)
- **`server` mode is for aggregation, not codegen.** Routes are served and their handlers consume APIs,
  but generating OpenAPI/SDKs from a server, and importing OpenAPI/GraphQL schemas, are not built.
- **Pipelines build a per-element scope by cloning.** Fine for typical API payloads; a huge array with a
  large environment would want a cheaper scope representation.
- **Expression-position `match` is synchronous.** A `match` used as a value (`let x = match …`) cannot do
  fetches in its arms; lift it to a statement. Statement-level `match` is fully async.
- **The runtime is a scheduling tree-walker, not a bytecode VM/JIT.** The optimizer's data-dependency
  DAG is the real artefact; a bytecode backend and adaptive JIT are future work, not claimed here.
```
