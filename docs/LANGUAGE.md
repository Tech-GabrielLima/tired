# TIRED — Language Reference

*The Internet Request & Execution Domain-language*

This is the complete reference manual for the TIRED language: every construct, its
syntax, its meaning, and what the compiler guarantees. It documents the language **as
implemented** — if something is described here, it compiles and runs. For the design
rationale and the internals of each compiler stage, see [DESIGN.md](DESIGN.md); for the
formal grammar, see [grammar.ebnf](grammar.ebnf).

> **Mental model.** A TIRED program is a small script that *consumes* (and optionally
> *serves*) HTTP APIs. You write straight-line code; the compiler type-checks your error
> handling, then the optimizer rewrites your requests into the fastest *safe* schedule —
> running independent calls in parallel, collapsing duplicates, and dropping calls whose
> results you never use.

---

## Table of contents

1. [Getting started](#1-getting-started)
2. [Lexical structure](#2-lexical-structure)
3. [Program structure](#3-program-structure)
4. [Endpoints](#4-endpoints)
5. [Types and contracts](#5-types-and-contracts)
6. [Fetch statements](#6-fetch-statements)
7. [Pipelines](#7-pipelines)
8. [Expressions](#8-expressions)
9. [Pattern matching and error handling](#9-pattern-matching-and-error-handling)
10. [Statements](#10-statements)
11. [Flows](#11-flows)
12. [Mocks](#12-mocks)
13. [Tests](#13-tests)
14. [Server mode](#14-server-mode)
15. [Compiler guarantees](#15-compiler-guarantees)
16. [The command-line tool](#16-the-command-line-tool)
17. [Tooling](#17-tooling)
18. [Diagnostics](#18-diagnostics)
19. [Limitations](#19-limitations)
20. [Appendix: keywords, operators, types](#20-appendix-keywords-operators-types)

---

## 1. Getting started

TIRED is a Rust workspace; the language is driven by the `tired` binary.

```bash
cargo build                                # builds the `tired` binary
alias tired="cargo run -q -p tired-cli --"

tired check   examples/broken.tired        # type-check, see diagnostics
tired test    examples/mocked.tired        # run test blocks, fully offline
tired explain examples/parallel.tired      # print the optimized execution plan
tired run     examples/parallel.tired      # actually execute (hits the network)
```

A minimal program declares an endpoint and fetches from it:

```tired
endpoint GitHub {
  base: "https://api.github.com"
}

fetch GitHub /users/octocat -> user
log "hello, {user.login}"
```

Top-level statements (those not inside a `flow`, `test`, or `server`) form the program's
**`main` body** — the entry point that `tired run` executes.

---

## 2. Lexical structure

### Comments

Only line comments, introduced by `//`, run to end of line. There are no block comments.

```tired
// this is a comment
fetch API /thing -> t   // trailing comments are fine too
```

### Whitespace

Whitespace and newlines are insignificant between tokens; they only separate tokens.
Indentation carries no meaning. TIRED has **no statement terminators** — statements end
where the next one begins. Commas inside record/array literals and `params` blocks are
optional.

### Identifiers

`ident = (letter | "_") { letter | digit | "_" }`. By convention, and as the checker
relies on it:

- **lower-case** identifiers are values: variables, fields, flow names, lambda params.
- **Upper-case** identifiers are types and constructors: `Repo`, `User`, `NotFound`,
  `Ok`, `Err`. The checker never flags an unknown upper-case name (it is assumed to be a
  type or error constructor); it *does* flag an unknown lower-case name used as a variable
  or path parameter, with a "did you mean?" suggestion.

A keyword may be reused as a *name* in positions where only a name is grammatical (e.g.
an endpoint setting key, a record field name). This is why `retry` can be both a keyword
inside a `match` arm and a setting key in an `endpoint`.

### Literals

| Kind | Examples | Notes |
|---|---|---|
| Integer | `0`, `42`, `999` | |
| Float | `3.14`, `0.5` | a `.` is a decimal point only when followed by a digit |
| String | `"hello"`, `"user {id}"` | double-quoted; supports interpolation (below) |
| Duration | `5s`, `300ms`, `5min`, `2h`, `30d` | normalized to milliseconds internally |
| Boolean | `true`, `false` | |
| Null | `null` | |

**Duration units:** `ms`, `s`, `m`, `min`, `h`, `d`. A number immediately followed by a
known unit is one duration token; `1..100` lexes as a range (`1 .. 100`), never `1.` then
`.100`.

### String interpolation

Inside a string, `{ expr }` splices a value; `{{` and `}}` are literal braces.

```tired
log "{user.login}: {top.length} repos"
log "literal braces: {{ not interpolated }}"
```

### Environment variables

`$NAME` reads an environment variable as a value — used most often for secrets in endpoint
config:

```tired
endpoint Stripe {
  base: "https://api.stripe.com/v1"
  auth: Bearer($STRIPE_KEY)
}
```

### Operators

Arithmetic `+ - *`, comparison `== != < <= > >=`, logical `and or not`, range `..`
(used only in `where (... in a..b)` constraints), the pipeline bar `|`, the bind arrow
`->`, the lambda arrow `=>`, and the union bar `|` in types. See the
[precedence table](#operator-precedence) in the appendix.

---

## 3. Program structure

A program is a sequence of **items**. The item kinds are:

| Item | Keyword | Purpose |
|---|---|---|
| Endpoint | `endpoint` | declare a base URL + per-host policy (auth, retry, cache, …) |
| Type | `type` | a record shape used for field-typing and checks |
| Contract | `contract` | like `type`, but its `where` constraints are verified at runtime |
| Flow | `flow` | a named, parameterized sub-program (a "function" that does I/O) |
| Mock | `mock` | an in-language fake of an endpoint for offline tests |
| Test | `test` | a named, asserted scenario run by `tired test` |
| Server | `server` | HTTP routes whose handlers consume APIs (server mode) |
| Statement | — | bare statements at top level form the `main` body |

Items may appear in any order; forward references between declarations are fine.

---

## 4. Endpoints

An `endpoint` names a base URL and the policy applied to every request to that host.

```tired
endpoint GitHub {
  base:    "https://api.github.com"
  auth:    Bearer($GITHUB_TOKEN)
  timeout: 5s
  retry:   3 backoff(exponential)
  cache:   ttl(5min)
}
```

A setting is `name: value [value ...]` — one key, one or more values. The recognized
settings:

| Setting | Form | Meaning |
|---|---|---|
| `base` | `"https://host/prefix"` | URL prefix prepended to every fetch path |
| `auth` | `Bearer($TOKEN)` | sends `Authorization: Bearer <token>`; the token comes from the env var |
| `timeout` | `5s`, `800ms` | per-request timeout (a duration) |
| `retry` | `3 backoff(exponential)` | retry count plus a backoff policy: `backoff(exponential)` or `backoff(constant)` |
| `cache` | `ttl(5min)` | cache successful responses for the given TTL |

Retries and caching apply **only to idempotent (GET/HEAD) requests** — see
[mutation safety](#mutation-safety). An unknown endpoint name in a `fetch` is a compile
error with a "did you mean?" over the declared endpoints.

---

## 5. Types and contracts

### Built-in scalar types

| Type | Meaning |
|---|---|
| `String` | text |
| `Integer` | integer (`Int` is accepted as a synonym) |
| `Float` | floating-point number (`Number` accepted as a synonym) |
| `Bool` | boolean (`Boolean` accepted as a synonym) |
| `Duration` | a duration literal (`5s`, …) |
| `Null` | the null value |

### Semantic (refinement) types

Refinements of `String` that carry intent and map to JSON-Schema `format`s on export, and
that `tired inspect` infers from samples:

`Url` · `Email` · `DateTime` · `UUID`

### Composite types

| Form | Meaning |
|---|---|
| `T[]` | array of `T` |
| `T?` | optional `T` (may be null) |
| `Result<T, E>` | a fallible value: success `T` or an error in domain `E` |
| `A \| B` | a union — used most often as a **closed error domain** |

`Result` and unions are what power compile-time error handling — see
[§9](#9-pattern-matching-and-error-handling).

### Record types

A `type` declares a named record. Fields are `name: type` (commas optional). A field may
carry a `where (...)` constraint:

```tired
type User {
  login: String
  id:    Integer
}

type Repo {
  name:             String
  stargazers_count: Integer where (>= 0)
}
```

Field names must be unique within a record; a duplicate is a compile error. Declaring a
`type` lets the checker validate field access (`x.field`) and pipeline predicates against
the element type — including "did you mean?" suggestions for typos.

A record type may be **scoped to an endpoint** with `type Name.Field { … }` syntax
(see the grammar's `["." ident]`), associating a shape with a specific route family.

### Contracts

A `contract` is declared exactly like a `type`, but its constraints are **checked at
runtime** against responses. When a fetch is annotated with a contract type, the response
is validated field-by-field; only declared constraints bite (extra/unknown fields are
tolerated, so contracts don't break when an API adds fields).

```tired
contract Repo {
  id:    Integer where (> 0)
  name:  String  where (length in 1..100)
  stars: Integer where (>= 0)
}
```

### Constraints (`where`)

A constraint follows a field type and refines its allowed values:

| Form | Meaning |
|---|---|
| `where (> 0)`, `where (>= 0)`, `where (< n)`, `where (<= n)`, `where (== v)`, `where (!= v)` | comparison against a value |
| `where (in a..b)` | value within the inclusive range `a..b` |
| `where (length in 1..100)` | the same, applied to the value's **length** (string length / array size) |
| `where (length > 0)` | a comparison applied to the length |

Constraints surface in two places: `tired schema` turns them into JSON-Schema keywords
(`minimum`, `maxLength`, …), and the runtime contract verifier enforces them on responses.

---

## 6. Fetch statements

`fetch` is the core statement: it issues an HTTP request and binds the result.

```
fetch [METHOD] Endpoint /path
      [body <expr>]
      [params { name: expr, ... }]
      { | pipeline_op }
      [-> binding]
```

- **METHOD** is optional and defaults to `GET`. The verbs are `GET`, `POST`, `PUT`,
  `PATCH`, `DELETE`, `HEAD`, `OPTIONS`.
- **`/path`** is appended to the endpoint's `base`. Path segments may be identifiers,
  integers, or `{expr}` interpolations:

  ```tired
  fetch GitHub /users/{username}/repos -> repos
  fetch Store  /orders/{created.id}    -> order   // any expression in braces
  ```

- **`body <expr>`** sends a JSON request body (typically a record literal). Use it with
  the mutating verbs:

  ```tired
  fetch POST Store /orders body { item: item, qty: 2 } -> created
  ```

- **`params { … }`** adds query-string parameters.
- **`-> binding`** names the result; an optional type annotation opts the result into
  checking: `-> repos: Repo[]`, `-> user: User`, `-> r: Result<Charge, ApiError>`. A
  fetch with no binding is still executed for its effect (a non-GET) but its result is
  unobservable — for a GET, that triggers [dead-request elimination](#dead-request-elimination).

### Result-annotated fetches

Annotating a fetch as `Result<T, E>` is how you opt into **checked error handling**: the
binding is then a value you must `match` (or `return` to propagate). Reading a field off
it directly is a compile error. See [§9](#9-pattern-matching-and-error-handling).

```tired
fetch Stripe /charges/{id} -> result: Result<Charge, ApiError>
```

A fetch *without* a `Result` annotation unwraps success automatically; if the request
fails at runtime, it raises a runtime error (with a hint to annotate it as `Result<...>`
if you want to handle the failure in-language).

---

## 7. Pipelines

A `|` after a fetch (or after another pipeline op) transforms the response in-language,
before it is bound. Pipelines are pure data transforms over arrays.

```tired
fetch GitHub /users/{u}/repos
  | filter(repo => repo.stargazers_count > 100)
  | sort(by: .stargazers_count desc)
  | limit(3)
  -> top: Repo[]
```

The full operator set:

| Operator | Form | Effect |
|---|---|---|
| `filter` | `filter(x => cond)` | keep elements where the predicate holds |
| `map` | `map(x => expr)` | transform each element |
| `pluck` | `pluck(.field)` | sugar for `map(x => x.field)` — project one field |
| `sort` | `sort(by: .field [asc\|desc])` | sort by a key; `asc` is the default |
| `limit` / `take` | `limit(n)` / `take(n)` | keep the first `n` |
| `skip` | `skip(n)` | drop the first `n` |
| `reverse` | `reverse()` | reverse order |
| `unique` | `unique()` or `unique(by: .field)` | de-duplicate (optionally by a key) |
| `flatten` | `flatten()` | flatten one level of nested arrays (no-op on a flat array) |
| `count` | `count()` | **terminal**: the number of elements |
| `sum` | `sum()` or `sum(by: .field)` | **terminal**: the sum (optionally of a field) |

Inside a pipeline op, `.field` is shorthand for "the field of the current element", and a
lambda `x => …` binds the element explicitly. The checker validates these against the
element type derived from the binding's annotation.

```tired
fetch API /nums | sort(by: .v desc) | pluck(.v) | unique() -> vs   // [3,2,1]
fetch API /nums | sum(by: .v)  -> total                            // 10
fetch API /nums | count()      -> n                                // 5
```

---

## 8. Expressions

### Operands

- Literals (`§2`): integers, floats, strings, durations, `true`/`false`/`null`.
- Variables and flow names (`ident`).
- `$NAME` — an environment variable.
- `.field` — the current pipeline element's field (only meaningful inside a pipeline op).
- Record literals, array literals, `match` expressions, parenthesized expressions.

### Field access and calls

- `value.field` — record/object field access. Chains: `a.b.c`.
- `f(arg, …)` — a call. Used for flow calls (`Dashboard("octocat")`) and built-in
  methods on values.

### Built-in value methods

Properties and methods available on values (postfix, like field access / calls):

| Member | On | Result |
|---|---|---|
| `.length` | arrays, strings | element count / character count |
| `.all(x => cond)` | arrays | `true` if every element satisfies the predicate |

```tired
assert repos.length == 2
assert repos.all(r => r.stars > 100)
```

### Lambdas

`param => expr` — an anonymous function, used by pipeline operators and `.all`:

```tired
filter(repo => repo.stars > 100)
```

The lambda parameter shadows outer bindings within its body and is excluded from the
dependency analysis (it isn't a free variable read).

### Record literals

`{ name: expr, … }` builds an object (commas optional). Prefix it with a type name for a
**named record literal**: `User { login: "octocat" }`.

```tired
return { login: profile.login, repos: top.length }
```

### Array literals and spread

`[ a, b, c ]` builds an array; `...expr` splices another array's elements in:

```tired
let xs = [1, 2, 3]
let ys = [0, ...xs, 4]   // [0,1,2,3,4]
```

### String interpolation

As in `§2`: `"{expr} text {expr}"`.

---

## 9. Pattern matching and error handling

This is TIRED's flagship feature: **network-dependent error handling, checked at compile
time.** A `Result`-typed value is unusable as if it had succeeded — you must `match` it.

### `match` expressions

```tired
match result {
  Ok(charge)         => charge
  Err(NotFound)      => fallback()
  Err(RateLimit(ms)) => wait(ms) then retry
  Err(e)             => fallback()
}
```

- A `match` has one or more arms `pattern => body`.
- An arm body is an **expression**, a **block** `{ … }`, or a **retry chain** (below).
- A statement-level `match` may do I/O in its arms (fetches, flow calls). A `match` used
  as a value (`let x = match …`) must be synchronous — lift it to a statement if an arm
  needs to fetch.

### Patterns

| Pattern | Matches |
|---|---|
| `_` | anything (wildcard) |
| `name` (lower-case) | anything, binding it to `name` |
| `Ok(p)` | a success, binding the payload to pattern `p` |
| `Err(NotFound)` | a specific nullary error variant |
| `Err(RateLimit(ms))` | an error variant carrying a payload, bound to `ms` |
| `Err(e)` | any error, binding it to `e` (the catch-all for an open domain) |
| `Constructor(p, …)` | a constructor with sub-patterns |

A bare upper-case name is a **nullary constructor** (a specific variant); a lower-case
name is a **binding**.

### Exhaustiveness

A `match` on a `Result` **must be exhaustive**:

- a **closed error union** `Result<T, NotFound | Unauthorized>` requires each variant to
  be handled (or a catch-all);
- an **open error type** `Result<T, ApiError>` requires a catch-all `Err(e) => …`.

The two errors that the checker raises around `Result` values:

```text
error: cannot read field `name` — `maybe` is a `Result<Repo, ?>`
   = help: `match` on it first and read the field inside the `Ok(...)` arm

error: unhandled error: `maybe` has type `Result<Repo, ?>` and may be an `Err`
   = help: `match maybe { ... }` and handle both `Ok` and `Err`, or `return maybe` to propagate it
```

### Retry chains

A `match` arm body may be a **retry chain**: zero or more `(call | wait(expr)) then`
steps, ending in `retry`. It runs the steps (e.g. wait out a rate-limit) and then
**re-runs the fetch that produced the scrutinee**, capped at a fixed number of attempts.

```tired
Err(RateLimit(ms)) => wait(ms) then retry
```

---

## 10. Statements

Statements appear in the `main` body, in `flow` bodies, in `test` bodies, in `match`-arm
blocks, and in `server` route handlers.

| Statement | Form | Meaning |
|---|---|---|
| Fetch | `fetch … -> b` | issue a request (`§6`) |
| Let | `let name = expr` | bind a local value |
| Log | `log expr` | print a value (an **effect** — order preserved) |
| Return | `return [expr]` | return from a flow / route handler (an effect) |
| Assert | `assert expr` | fail the enclosing test if false (an effect) |
| Parallel | `parallel { … }` | a block whose statements are explicitly run concurrently |
| Using mock | `using mock Name` | inside a test, route this endpoint's calls to a `mock` |
| Expression | `expr [-> binding]` | evaluate an expression, optionally binding it (e.g. a flow call: `Dashboard("octocat") -> dash`) |

Note that **automatic parallelism** ([§15](#parallel-inference)) usually makes an explicit
`parallel` block unnecessary — the optimizer already runs independent fetches
concurrently. `parallel` is there for when you want to state the intent explicitly.

`log`, `return`, `assert`, flow calls, and `match` are **effect** statements: they are
sequenced in program order relative to one another, so observable behavior (e.g. the order
of `log` output) is preserved even as pure computations are reordered around them.

---

## 11. Flows

A `flow` is a named, parameterized sub-program — TIRED's unit of reuse. It can take typed
parameters, optionally declare a return type, and is called like a function.

```tired
flow Dashboard(username: String) -> User {
  fetch GitHub /users/{username} -> user: User

  fetch GitHub /users/{username}/repos
    | sort(by: .stargazers_count desc) | limit(3) -> top

  fetch GitHub /users/{username}/followers | limit(3) -> followers

  log "{user.login}: {top.length} repos, {followers.length} followers"
  return user
}

// call it from the main body:
Dashboard("octocat") -> dash
```

- Parameters are `name: type`, comma-separated.
- The optional `-> type` after the parameter list is the return type.
- A flow's body is optimized independently: its own dependency graph is scheduled into
  waves, so a flow's internal fan-out parallelizes (here, `repos` and `followers` run
  concurrently because both depend only on `username`, not on each other).
- Flows may call other flows; recursion is allowed (the runtime boxes recursive async
  calls). Static request-cost analysis breaks recursion with a guard.

Run a specific flow from the CLI, passing string arguments:

```bash
tired run examples/github_dashboard.tired --flow Dashboard octocat
```

---

## 12. Mocks

A `mock` block is an in-language fake of an endpoint, making `tired test` fully offline and
deterministic. It is a routing table keyed by **method + path**.

```tired
mock GitHub {
  GET /users/{user}/repos -> [
    { id: 1, name: "alpha", stars: 250 },
    { id: 2, name: "beta",  stars: 12 }
  ]
  GET /repos/999 -> NotFound          // an error variant => a typed failure
}
```

- Each route is `METHOD /path -> response`. The response is any expression — a record, an
  array, or the name of an **error variant** (`NotFound`, `RateLimit(ms)`, …), which makes
  the mocked call resolve to a typed `Err`.
- Path parameters are captured and exposed to the response expression as `$name`:

  ```tired
  GET /charges/{id} -> { id: $id, amount: 4200, currency: "usd" }
  ```

- **Order matters:** a more specific route must precede a parameterized one
  (`GET /charges/missing` before `GET /charges/{id}`), because routes are matched
  top-to-bottom.

A mock is activated inside a test with `using mock Name`.

---

## 13. Tests

A `test` is a named scenario with assertions, run by `tired test`. Tests are the offline,
deterministic way to exercise a program — they typically activate a mock first.

```tired
test "keeps only popular repos, most-starred first" {
  using mock GitHub

  fetch GitHub /users/gabriel/repos
    | filter(repo => repo.stars > 100)
    | sort(by: .stars desc)
    -> repos: Repo[]

  assert repos.length == 2
  assert repos.all(r => r.stars > 100)
}
```

- A test body is a normal block: it may fetch, call flows, `let`, `log`, and `assert`.
- `using mock Name` redirects that endpoint's requests to its `mock` block for the
  duration of the test.
- Each `assert expr` must evaluate truthy; the first failing assertion fails the test with
  a message.

`tired test <file>` runs every `test` in the file and prints a summary:

```text
test result: ok — 2 passed, 0 failed (of 2)
```

---

## 14. Server mode

A `server` block turns TIRED around: instead of only *consuming* APIs, it *serves* HTTP
routes. Each route handler is ordinary TIRED code — so the **same optimizer parallelizes
and deduplicates its upstream calls**. This makes TIRED an API gateway / backend-for-
frontend whose concurrency the compiler writes for you.

```tired
endpoint GitHub {
  base:    "https://api.github.com"
  timeout: 8s
  cache:   ttl(30s)
}

server Gateway {
  port: 8088
  base: "/api"

  route GET /dashboard/{user} -> {
    fetch GitHub /users/{user}            -> profile
    fetch GitHub /users/{user}/repos
      | sort(by: .stargazers_count desc) | limit(3) -> top
    fetch GitHub /users/{user}/followers  | limit(3) -> followers

    return {
      login:     profile.login,
      top_repos: top.length,
      followers: followers.length
    }
  }
}
```

### Server settings

| Setting | Meaning |
|---|---|
| `port` | TCP port to listen on (overridable with `--port`) |
| `base` | a path prefix stripped before route matching (here, `/api`) |

### Routes

`route METHOD /path -> ( block | expr )`. Inside a route handler these bindings are in
scope:

- **path parameters** — `{user}` becomes a `user` binding;
- **`query`** — the request's query string as an object;
- **`body`** — the parsed JSON request body.

The handler runs through the full executor: fan-out fetches in a handler are scheduled into
parallel waves, identical requests are deduplicated, and the result is serialized to JSON
as the HTTP response (`200` on success, `404` for an unmatched route, `500` on a handler
error).

### Running it

```bash
tired explain examples/gateway.tired       # plan + request cost (no network)
tired serve   examples/gateway.tired        # serve on http://127.0.0.1:8088/api/...
tired serve   examples/gateway.tired Gateway --port 9090
```

If a file declares a single `server`, `tired serve` picks it automatically; otherwise name
it.

---

## 15. Compiler guarantees

These are the semantic guarantees the compiler and optimizer give you. They apply to every
body (main, flows, route handlers, match-arm blocks). See [DESIGN.md](DESIGN.md) for how
each is implemented.

### Parallel inference

Each body is lowered to a dependency graph (a node depends on the most recent node that
wrote a variable it reads). Nodes are levelled topologically into **waves**; nodes in the
same wave are mutually independent and **execute concurrently**. You write sequential
`fetch`es; the optimizer runs the independent ones in parallel. `tired explain` prints the
waves:

```text
main:
  wave 1:  ‖ 3 requests in parallel
    • fetch GitHub /users/torvalds -> a
    • fetch GitHub /users/octocat -> b
    • fetch GitHub /users/gvanrossum -> c
  wave 2:
    • log
```

### Dead-request elimination

A GET whose result is never observed (never read, never returned, never logged) is
**removed before any byte is sent**, and reported as a warning. Liveness is backward
reachability from the effect nodes.

```text
warning: request `GitHub /users/torvalds/repos` is never used and was eliminated
   = note: dead-request elimination: 0 bytes were sent for it
```

### Request deduplication (CSE)

Two GETs that issue the **identical** request — same endpoint, path, params, pipeline, and
the same inputs — are collapsed: the later one is rewritten into a `let` that reuses the
first's result. The network is hit once. It is common-subexpression elimination for HTTP.

### Mutation safety

A non-GET fetch is a **mutation**, treated as an effect. The optimizer therefore **never**
reorders, deduplicates, eliminates, or auto-retries a mutation — a `POST`/`PUT`/`PATCH`/
`DELETE` may already have been received by the server, so repeating or dropping it is
unsafe. Only GET/HEAD requests are freely parallelized, cached, deduped, and retried. The
compiler knows the difference between a safe read and a side effect.

### Static request-cost analysis

Walking the optimized IR, the compiler bounds **how many network requests any path through
a flow or route can issue**, and **how many run in parallel** — a `match` takes the *max*
over its arms; a flow call adds that flow's cost; recursion is bounded. It is surfaced by
`tired explain`:

```text
server Gateway:
  route GET /dashboard/{..}:  [≤ 3 requests, up to 3 in parallel]
```

No HTTP client tells you the blast radius of an endpoint at compile time. TIRED reads it
off the IR.

---

## 16. The command-line tool

```text
tired run     <file> [--flow NAME [arg ...]] [--show-plan] [--metrics]
                     [--record <rec.json>] [--replay <rec.json>]
tired check   <file>
tired fmt     <file> [--write]
tired test    <file>
tired explain <file>                         (alias: tired plan)
tired inspect <url|file.json> [TypeName]     # infer TIRED types from JSON
tired schema  <file> [--title T]             # export types/contracts as JSON Schema
tired serve   <file> [Server] [--port N]     # run a `server` block over HTTP
tired replay  <rec.json> <file>              # re-run offline from a recording
tired lsp                                    # run the language server (stdio)
tired version | --version | -V
tired help    | --help    | -h
```

| Command | What it does |
|---|---|
| `run` | compile and execute the program (or a named `--flow`). `--show-plan` prints the wave plan; `--metrics` prints request/cache/retry/error counts. |
| `check` | type-check only; print diagnostics and a pass/fail summary. |
| `fmt` | canonical formatting via the pretty-printer; `--write` rewrites the file in place. |
| `test` | run every `test` block and report passed/failed. |
| `explain` | print the optimized execution plan (waves + request cost) without running. |
| `inspect` | infer TIRED `type` declarations from a JSON sample (a URL or a `.json` file). |
| `schema` | emit JSON Schema (2020-12) for the file's `type`/`contract` declarations. |
| `serve` | run a `server` block as a live HTTP server. |
| `replay` | re-run a program against a saved recording (offline, deterministic). |
| `lsp` | start the language server on stdio (for editor integration). |

---

## 17. Tooling

### Formatter (`tired fmt`)

A canonical pretty-printer round-trips any valid program to a normalized form. `--write`
edits in place; without it, the formatted source is printed.

### Schema inference (`tired inspect`)

Given a JSON sample, reconstructs TIRED `type` declarations: objects become typed records,
arrays of objects become `Elem[]` with a *merged* element type (a field present in only
some elements is marked optional), and strings get semantic types (`Url`, `Email`,
`DateTime`, `UUID`) by light heuristics.

```bash
tired inspect https://api.github.com/users/octocat User
tired inspect ./sample.json Payload
```

### JSON Schema export (`tired schema`)

Emits JSON Schema 2020-12 from declared `type`/`contract`s — field types map to JSON-Schema
types (with `format`s for `Url`/`Email`/…), and `where` constraints become `minimum` /
`maxLength` / … keywords.

### Record & replay (time-travel)

Record every request's outcome once against the live API, then replay forever — offline and
deterministic. A missing key in a replay is a hard error, so a replay is reproducible.

```bash
tired run    examples/parallel.tired --record session.json   # capture (live)
tired replay session.json examples/parallel.tired            # reproduce (offline)
```

### Language server (`tired lsp`)

A stdio LSP that runs the compiler on every edit and publishes the same diagnostics the CLI
prints, plus keyword/endpoint **completion** and **hover**. Point any LSP client at
`tired lsp`. A **VS Code extension** (`editors/vscode`) packages it with a TextMate grammar
for syntax highlighting.

### Python bindings (PyO3)

The compiler + runtime are exposed to Python as a single `abi3` wheel (works on CPython
3.8+), installable with `maturin`:

```bash
pip install maturin && (cd crates/tired-py && maturin develop)
```

```python
import tired
tired.is_valid(src)                       # -> bool
tired.check(src)                          # -> list of diagnostic strings
tired.explain(src)                        # -> the execution plan
tired.run(src)                            # -> run the program
tired.inspect('{"id":1}', "User")         # -> inferred TIRED types
tired.json_schema(src, "API")             # -> JSON Schema
```

---

## 18. Diagnostics

TIRED's diagnostics follow `rustc`'s style: a message, a primary span with a caret, and
`help`/`note` lines. Typos in field names and endpoint names get a "did you mean?"
suggestion computed with optimal-string-alignment (Levenshtein-with-transposition)
distance.

```text
error: no field `starz` on type `Repo`
  --> examples/broken.tired:15:25
   |
15 |   | filter(repo => repo.starz > 100)
   |                         ^^^^^
   = help: did you mean `stars`?
```

The guiding principle is **no false positives**: a check fires only when the type
information needed to justify it is actually present. Without a binding annotation, a fetch
result is `Unknown`, and field/exhaustiveness checks don't apply to it — annotating is how
you opt into stricter checking.

Errors fail the build; warnings (e.g. dead-request elimination) do not. The parser recovers
to the next top-level item on a syntax error, so one typo doesn't cascade.

---

## 19. Limitations

Deliberate boundaries that keep the implementation honest:

- **Type inference is annotation-driven.** A fetch without a binding annotation is
  `Unknown`; opt into `Result<...>`/record types to get checked error handling and field
  typing. The checker does not infer response shapes from the network (`tired inspect`
  generates types offline from a sample instead).
- **`server` mode is for aggregation, not codegen.** Routes are served and their handlers
  consume APIs, but generating OpenAPI/SDKs from a server — and importing OpenAPI/GraphQL
  schemas — are designed, not built.
- **Expression-position `match` is synchronous.** A `match` used as a value can't fetch in
  its arms; lift it to a statement. Statement-level `match` is fully async.
- **No division operator.** Arithmetic is `+`, `-`, `*`; richer numeric expression is out
  of scope for an API-consumption DSL.
- **The runtime is a scheduling tree-walker, not a bytecode VM / JIT.** The optimizer's
  data-dependency DAG is the real artifact; a bytecode backend and adaptive JIT are future
  work.

---

## 20. Appendix: keywords, operators, types

### Keywords

```
endpoint  type  contract  flow  fetch  parallel  match  mock  test  using
assert    let   log       return  params  server  route  where  retry  wait
then      in    by        asc     desc    and     or     not    true   false  null
```

HTTP method words (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`) are
recognized contextually in `fetch`, `mock`, and `route` positions.

### Operator precedence

From lowest to highest binding:

| Level | Operators | Associativity |
|---|---|---|
| 1 | `or` | left |
| 2 | `and` | left |
| 3 | `== != < <= > >=` | left |
| 4 | `+ -` | left |
| 5 | `*` | left |
| 6 | `not` `-` (unary) | prefix |
| 7 | `.field`, `(call)` (postfix) | left |

The pipeline `|`, the bind `->`, and the lambda `=>` are not arithmetic operators; they are
structural and bind at the statement/clause level.

### Type forms at a glance

```
String  Integer (Int)  Float  Bool  Duration  Null     -- scalars
Url  Email  DateTime  UUID                              -- semantic refinements of String
T[]            -- array of T
T?             -- optional T
Result<T, E>   -- fallible: Ok(T) or an error in E
A | B          -- union (closed error domain)
RecordName     -- a declared `type` / `contract`
```

### Built-in members

| Member | On | Result |
|---|---|---|
| `.length` | array, string | count |
| `.all(p)` | array | all elements satisfy `p` |

### Error constructors (in patterns / mocks)

`Ok(x)` · `Err(e)` · `Err(NotFound)` · `Err(Unauthorized)` · `Err(RateLimit(ms))` — and any
upper-case name you use as a nullary error variant in a closed union or a `mock` response.

---

*This reference documents TIRED as implemented in this repository. For the design rationale
and the per-stage internals, see [DESIGN.md](DESIGN.md); for the formal grammar, see
[grammar.ebnf](grammar.ebnf); for runnable programs, see [`examples/`](../examples).*
