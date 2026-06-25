//! Semantic analysis. Three families of checks, all built to avoid false positives —
//! a check only fires when the necessary type information is actually present:
//!
//! 1. **Resolution** — `fetch` endpoints and variables resolve, with "did you mean?".
//! 2. **Field typing** — `.field` access on a declared record (and on pipeline
//!    elements) is validated against the record's fields.
//! 3. **Network-dependent error handling** (the flagship) — a `Result<T, E>` binding
//!    must be `match`ed (or returned to propagate), the `match` must be exhaustive over
//!    `Ok`/`Err`, and a field cannot be read off a value that might be an `Err`.

use std::collections::{HashMap, HashSet};

use tired_syntax::ast::*;
use tired_syntax::diag::{did_you_mean, Diagnostic, Diagnostics};
use tired_syntax::span::Span;

use crate::types::{ErrDomain, RecordField, Type, TypeTable};

pub struct Analysis {
    pub table: TypeTable,
    pub endpoints: Vec<String>,
    pub flows: Vec<String>,
}

pub fn check(program: &Program) -> (Analysis, Diagnostics) {
    let mut ck = Checker::new(program);
    ck.check_declarations(program);
    ck.run(program);
    ck.finish()
}

type Scope = Vec<HashMap<String, Type>>;

struct Checker {
    table: TypeTable,
    endpoints: HashSet<String>,
    flows: HashSet<String>,
    type_names: HashSet<String>,
    diags: Diagnostics,
    /// `Result`-typed bindings: (name, type, span). Checked for handling at the end.
    results: Vec<(String, Type, Span)>,
    /// Names ever used as a `match` scrutinee, or returned directly (= propagated).
    handled: HashSet<String>,
}

/// Bare identifiers that are language constants / config enums rather than variables.
const CONSTANTS: &[&str] = &[
    "exponential",
    "linear",
    "constant",
    "jitter",
    "opentelemetry",
    "prometheus",
    "redis",
    "none",
    "lowest_latency",
    "round_robin",
    "weighted",
    "geo",
];
/// Built-in callables and constructors (referencing these by name is never an error).
const BUILTINS: &[&str] = &[
    "Ok",
    "Err",
    "Some",
    "None",
    "Bearer",
    "Basic",
    "ApiKey",
    "Token",
    "ttl",
    "header",
    "backoff",
    "prometheus",
    "opentelemetry",
    "fake",
    "now",
    "uuid",
    "env",
];

impl Checker {
    fn new(program: &Program) -> Self {
        let mut table = TypeTable::new();
        let mut endpoints = HashSet::new();
        let mut flows = HashSet::new();
        let mut type_names = HashSet::new();

        // First pass: collect declarations so forward references resolve.
        for item in &program.items {
            match item {
                Item::Endpoint(e) => {
                    endpoints.insert(e.name.node.clone());
                }
                Item::Flow(f) => {
                    flows.insert(f.name.node.clone());
                }
                Item::Type(t) => {
                    type_names.insert(t.name.node.clone());
                }
                _ => {}
            }
        }
        // Second pass: now that all type names are known, resolve field types.
        for item in &program.items {
            if let Item::Type(t) = item {
                let fields = t
                    .fields
                    .iter()
                    .map(|f| RecordField {
                        name: f.name.node.clone(),
                        ty: table.resolve(&f.ty),
                        decl: f.clone(),
                    })
                    .collect();
                table.declare(t.name.node.clone(), fields);
            }
        }

        Checker {
            table,
            endpoints,
            flows,
            type_names,
            diags: Diagnostics::new(),
            results: Vec::new(),
            handled: HashSet::new(),
        }
    }

    fn finish(mut self) -> (Analysis, Diagnostics) {
        // Flagship rule: every Result-typed binding must be handled.
        let results = std::mem::take(&mut self.results);
        for (name, ty, span) in results {
            if !self.handled.contains(&name) {
                self.diags.push(
                    Diagnostic::error(
                        span,
                        format!("unhandled error: `{name}` has type `{}` and may be an `Err`", ty.display()),
                    )
                    .with_help(format!(
                        "`match {name} {{ ... }}` and handle both `Ok` and `Err`, or `return {name}` to propagate it"
                    ))
                    .with_note("in TIRED a fallible result cannot be silently ignored"),
                );
            }
        }
        let analysis = Analysis {
            table: self.table,
            endpoints: self.endpoints.into_iter().collect(),
            flows: self.flows.into_iter().collect(),
        };
        (analysis, self.diags)
    }

    /// Structural checks on declarations themselves (independent of bodies).
    fn check_declarations(&mut self, program: &Program) {
        for item in &program.items {
            if let Item::Type(t) = item {
                let mut seen: HashSet<String> = HashSet::new();
                for f in &t.fields {
                    if !seen.insert(f.name.node.clone()) {
                        self.diags.push(Diagnostic::error(
                            f.name.span,
                            format!("duplicate field `{}` in `{}`", f.name.node, t.name.node),
                        ));
                    }
                }
            }
        }
    }

    fn run(&mut self, program: &Program) {
        // The top-level statements form an implicit "main".
        let mut scope: Scope = vec![HashMap::new()];
        for item in &program.items {
            if let Item::Stmt(s) = item {
                self.check_stmt(s, &mut scope);
            }
        }
        for item in &program.items {
            match item {
                Item::Flow(f) => self.check_flow(f),
                Item::Test(t) => {
                    let mut s: Scope = vec![HashMap::new()];
                    self.check_block(&t.body, &mut s);
                }
                _ => {}
            }
        }
    }

    fn check_flow(&mut self, f: &FlowDecl) {
        let mut scope: Scope = vec![HashMap::new()];
        for p in &f.params {
            let ty = self.table.resolve(&p.ty);
            self.bind(&mut scope, &p.name.node, ty);
        }
        self.check_block(&f.body, &mut scope);
    }

    // ---------- statements ----------

    fn check_block(&mut self, block: &Block, scope: &mut Scope) {
        scope.push(HashMap::new());
        for s in &block.stmts {
            self.check_stmt(s, scope);
        }
        scope.pop();
    }

    fn check_stmt(&mut self, stmt: &Stmt, scope: &mut Scope) {
        match stmt {
            Stmt::Fetch(f) => self.check_fetch(f, scope),
            Stmt::Let { name, value, .. } => {
                let ty = self.infer(value, scope, None);
                self.bind(scope, &name.node, ty);
            }
            Stmt::Log { value, .. } => {
                self.infer(value, scope, None);
            }
            Stmt::Parallel { block, .. } => {
                // A parallel block shares the enclosing scope; bindings flow outward.
                for s in &block.stmts {
                    self.check_stmt(s, scope);
                }
            }
            Stmt::Return { value, .. } => {
                if let Some(v) = value {
                    if let Expr::Ident(n) = v {
                        self.handled.insert(n.node.clone());
                    }
                    self.infer(v, scope, None);
                }
            }
            Stmt::Assert { value, .. } => {
                self.infer(value, scope, None);
            }
            Stmt::UsingMock { .. } => {}
            Stmt::Expr { expr, bind, .. } => {
                let ty = self.infer(expr, scope, None);
                if let Some(b) = bind {
                    let bound = b.ty.as_ref().map(|t| self.table.resolve(t)).unwrap_or(ty);
                    self.bind(scope, &b.name.node, bound);
                }
            }
        }
    }

    fn check_fetch(&mut self, f: &FetchStmt, scope: &mut Scope) {
        // Endpoint must resolve.
        if !self.endpoints.contains(&f.endpoint.node) {
            let cands: Vec<&str> = self.endpoints.iter().map(|s| s.as_str()).collect();
            let mut d = Diagnostic::error(
                f.endpoint.span,
                format!("unknown endpoint `{}`", f.endpoint.node),
            );
            if let Some(m) = did_you_mean(&f.endpoint.node, cands) {
                d = d.with_help(format!("did you mean `{m}`?"));
            } else {
                d = d.with_help("declare it with `endpoint <Name> { base: \"...\" }`");
            }
            self.diags.push(d);
        }

        // Path parameters and query params reference values in scope.
        for seg in &f.path.segments {
            if let PathSeg::Param(e) = seg {
                self.infer(e, scope, None);
            }
        }
        for (_, v) in &f.params {
            self.infer(v, scope, None);
        }

        // Determine the binding type and the pipeline element type.
        let binding_ty = f
            .bind
            .as_ref()
            .and_then(|b| b.ty.as_ref())
            .map(|t| self.table.resolve(t))
            .unwrap_or(Type::Unknown);
        let elem = binding_ty.element().unwrap_or(Type::Unknown);

        for op in &f.pipeline {
            self.check_pipeline_op(op, scope, &elem);
        }

        if let Some(b) = &f.bind {
            if binding_ty.is_result() {
                self.results
                    .push((b.name.node.clone(), binding_ty.clone(), b.name.span));
            }
            self.bind(scope, &b.name.node, binding_ty);
        }
    }

    fn check_pipeline_op(&mut self, op: &PipelineOp, scope: &mut Scope, elem: &Type) {
        match op {
            PipelineOp::Filter { lambda, .. } | PipelineOp::Map { lambda, .. } => {
                self.check_lambda_or_pred(lambda, scope, elem);
            }
            PipelineOp::Sort { by, .. } => {
                self.infer(by, scope, Some(elem));
            }
            PipelineOp::Limit { count, .. } | PipelineOp::Skip { count, .. } => {
                self.infer(count, scope, None);
            }
            // `by` (when present) is evaluated against the element type.
            PipelineOp::Unique { by: Some(e), .. } | PipelineOp::Sum { by: Some(e), .. } => {
                self.infer(e, scope, Some(elem));
            }
            PipelineOp::Reverse { .. }
            | PipelineOp::Flatten { .. }
            | PipelineOp::Count { .. }
            | PipelineOp::Unique { by: None, .. }
            | PipelineOp::Sum { by: None, .. } => {}
        }
    }

    /// A pipeline predicate is either `x => <body>` (bind `x` to the element type) or a
    /// bare implicit-field expression like `.stars > 100`.
    fn check_lambda_or_pred(&mut self, lambda: &Expr, scope: &mut Scope, elem: &Type) {
        if let Expr::Lambda { param, body, .. } = lambda {
            scope.push(HashMap::new());
            self.bind(scope, &param.node, elem.clone());
            self.infer(body, scope, Some(elem));
            scope.pop();
        } else {
            self.infer(lambda, scope, Some(elem));
        }
    }

    // ---------- expression typing ----------

    fn infer(&mut self, e: &Expr, scope: &Scope, elem: Option<&Type>) -> Type {
        match e {
            Expr::Int(..) => Type::Int,
            Expr::Float(..) => Type::Float,
            Expr::Bool(..) => Type::Bool,
            Expr::Null(_) => Type::Null,
            Expr::Duration(..) => Type::Duration,
            Expr::Str { parts, .. } => {
                for p in parts {
                    if let StrPart::Interp(ex) = p {
                        self.infer(ex, scope, elem);
                    }
                }
                Type::String
            }
            Expr::EnvVar(_) => Type::String,
            Expr::Ident(n) => self.lookup(scope, &n.node).unwrap_or(Type::Unknown),
            Expr::ImplicitField(field) => {
                let base = elem.cloned().unwrap_or(Type::Unknown);
                self.field_type(&base, field, None)
            }
            Expr::Field { base, field, .. } => {
                let bt = self.infer(base, scope, elem);
                // An unknown lowercase identifier used as a receiver is a typo.
                if bt.is_unknown() {
                    if let Expr::Ident(n) = base.as_ref() {
                        self.check_var_ref(&n.node, n.span, scope);
                    }
                }
                let base_name = match base.as_ref() {
                    Expr::Ident(n) => Some(n.node.as_str()),
                    _ => None,
                };
                self.field_type(&bt, field, base_name)
            }
            Expr::Call { callee, args, .. } => {
                self.check_call(callee, args, scope, elem);
                Type::Unknown
            }
            Expr::Unary { op, rhs, .. } => {
                let t = self.infer(rhs, scope, elem);
                match op {
                    UnOp::Not => Type::Bool,
                    UnOp::Neg => t,
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let lt = self.infer(lhs, scope, elem);
                let _rt = self.infer(rhs, scope, elem);
                if op.is_comparison() || matches!(op, BinOp::And | BinOp::Or) {
                    Type::Bool
                } else {
                    lt
                }
            }
            Expr::Record { name, fields, .. } => {
                for (_, v) in fields {
                    self.infer(v, scope, elem);
                }
                match name {
                    Some(n) if self.table.is_record(&n.node) => Type::Record(n.node.clone()),
                    _ => Type::Unknown,
                }
            }
            Expr::Array { elems, .. } => {
                let mut et = Type::Unknown;
                for (i, x) in elems.iter().enumerate() {
                    let t = self.infer(x, scope, elem);
                    if i == 0 {
                        et = t;
                    }
                }
                Type::Array(Box::new(et))
            }
            Expr::Spread { expr, .. } => self.infer(expr, scope, elem),
            Expr::Lambda { body, .. } => {
                self.infer(body, scope, elem);
                Type::Unknown
            }
            Expr::Match(m) => {
                self.check_match(m, scope, elem);
                Type::Unknown
            }
            Expr::Range { lo, hi, .. } => {
                self.infer(lo, scope, elem);
                self.infer(hi, scope, elem);
                Type::Unknown
            }
        }
    }

    fn field_type(
        &mut self,
        base: &Type,
        field: &tired_syntax::span::Spanned<String>,
        base_name: Option<&str>,
    ) -> Type {
        match base {
            Type::Result(..) => {
                let subject = base_name
                    .map(|n| format!("`{n}`"))
                    .unwrap_or_else(|| "this value".into());
                self.diags.push(
                    Diagnostic::error(
                        field.span,
                        format!(
                            "cannot read field `{}` — {subject} is a `{}`",
                            field.node,
                            base.display()
                        ),
                    )
                    .with_help("`match` on it first and read the field inside the `Ok(...)` arm")
                    .with_note("the request might have failed; TIRED will not let you ignore that"),
                );
                Type::Unknown
            }
            Type::Record(name) => {
                if let Some(f) = self.table.field(name, &field.node) {
                    f.ty.clone()
                } else {
                    let mut d = Diagnostic::error(
                        field.span,
                        format!("no field `{}` on type `{name}`", field.node),
                    );
                    if let Some(m) = did_you_mean(&field.node, self.table.field_names(name)) {
                        d = d.with_help(format!("did you mean `{m}`?"));
                    }
                    self.diags.push(d);
                    Type::Unknown
                }
            }
            Type::Array(_) | Type::String | Type::Semantic(_) => {
                if field.node == "length" {
                    Type::Int
                } else {
                    Type::Unknown
                }
            }
            Type::Optional(inner) => self.field_type(inner, field, base_name),
            _ => Type::Unknown,
        }
    }

    fn check_call(&mut self, callee: &Expr, args: &[Expr], scope: &Scope, elem: Option<&Type>) {
        // Method-style calls on arrays bind their lambda to the element type, so e.g.
        // `repos.all(r => r.starz)` still type-checks the field access on `Repo`.
        if let Expr::Field { base, field, .. } = callee {
            let bt = self.infer(base, scope, elem);
            if matches!(
                field.node.as_str(),
                "all" | "any" | "map" | "filter" | "find" | "each" | "none"
            ) {
                if let Some(item) = bt.element() {
                    for a in args {
                        self.check_lambda_or_pred_ref(a, scope, &item);
                    }
                    return;
                }
            }
        } else {
            self.infer(callee, scope, elem);
        }
        for a in args {
            self.infer(a, scope, elem);
        }
    }

    fn check_lambda_or_pred_ref(&mut self, lambda: &Expr, scope: &Scope, elem: &Type) {
        if let Expr::Lambda { param, body, .. } = lambda {
            let mut s = scope.clone();
            s.push(HashMap::new());
            self.bind(&mut s, &param.node, elem.clone());
            self.infer(body, &s, Some(elem));
        } else {
            self.infer(lambda, scope, Some(elem));
        }
    }

    fn check_match(&mut self, m: &MatchExpr, scope: &Scope, elem: Option<&Type>) {
        let scrut_ty = self.infer(&m.scrutinee, scope, elem);
        if let Expr::Ident(n) = &m.scrutinee {
            self.handled.insert(n.node.clone());
        }

        // Type-check each arm body with the pattern's bindings in scope.
        for arm in &m.arms {
            let mut s = scope.clone();
            s.push(HashMap::new());
            self.bind_pattern(&arm.pattern, &scrut_ty, &mut s);
            self.check_arm_body(&arm.body, &mut s);
        }

        if let Type::Result(_, domain) = &scrut_ty {
            self.check_exhaustive(m, domain);
        }
    }

    fn check_arm_body(&mut self, body: &ArmBody, scope: &mut Scope) {
        match body {
            ArmBody::Value(e) => {
                self.infer(e, scope, None);
            }
            ArmBody::Block(b) => self.check_block(b, scope),
            ArmBody::Retry { effects, .. } => {
                for eff in effects {
                    match eff {
                        Effect::Call(e) | Effect::Wait(e) => {
                            self.infer(e, scope, None);
                        }
                    }
                }
            }
        }
    }

    fn bind_pattern(&mut self, pat: &Pattern, scrut: &Type, scope: &mut Scope) {
        match pat {
            Pattern::Binding(n) => self.bind(scope, &n.node, scrut.clone()),
            Pattern::Wildcard(_) => {}
            Pattern::Ctor { name, args, .. } => {
                // Inside `Ok(x)`, bind `x` to the success type of the Result.
                let inner = match (name.node.as_str(), scrut) {
                    ("Ok", Type::Result(t, _)) => (**t).clone(),
                    _ => Type::Unknown,
                };
                for a in args {
                    self.bind_pattern(a, &inner, scope);
                }
            }
        }
    }

    /// The heart of network-dependent error handling: a `match` on a `Result` must
    /// cover `Ok` and the full `Err` domain (or carry a catch-all).
    fn check_exhaustive(&mut self, m: &MatchExpr, domain: &ErrDomain) {
        let mut has_ok = false;
        let mut catch_all = false;
        let mut err_catch_all = false;
        let mut covered_err: HashSet<String> = HashSet::new();

        for arm in &m.arms {
            match &arm.pattern {
                Pattern::Wildcard(_) | Pattern::Binding(_) => catch_all = true,
                Pattern::Ctor { name, args, .. } => match name.node.as_str() {
                    "Ok" => has_ok = true,
                    "Err" => match args.first() {
                        Some(Pattern::Wildcard(_)) | Some(Pattern::Binding(_)) | None => {
                            err_catch_all = true
                        }
                        Some(Pattern::Ctor { name: v, .. }) => {
                            covered_err.insert(v.node.clone());
                        }
                    },
                    _ => {}
                },
            }
        }

        if catch_all {
            return; // a top-level `_`/binding covers everything
        }

        let mut missing: Vec<String> = Vec::new();
        if !has_ok {
            missing.push("Ok(...)".into());
        }
        if !err_catch_all {
            match domain {
                ErrDomain::Variants(vs) => {
                    for v in vs {
                        if !covered_err.contains(v) {
                            missing.push(format!("Err({v})"));
                        }
                    }
                }
                ErrDomain::Open => {
                    // An open error domain always needs a catch-all on the Err side.
                    missing.push("Err(e)".into());
                }
            }
        }

        if !missing.is_empty() {
            let help = if matches!(domain, ErrDomain::Open) {
                "the error type is open, so add a catch-all `Err(e) => ...` or `_ => ...` arm"
            } else {
                "handle every listed variant, or add a catch-all `Err(e) => ...` arm"
            };
            self.diags.push(
                Diagnostic::error(
                    m.span,
                    format!("non-exhaustive match: missing {}", missing.join(", ")),
                )
                .with_help(help)
                .with_note("a TIRED `match` on a Result must handle success and every failure"),
            );
        }
    }

    // ---------- name resolution helpers ----------

    fn check_var_ref(&mut self, name: &str, span: Span, scope: &Scope) {
        if self.resolvable(name, scope) {
            return;
        }
        let mut cands: Vec<String> = Vec::new();
        for frame in scope {
            cands.extend(frame.keys().cloned());
        }
        let refs: Vec<&str> = cands.iter().map(|s| s.as_str()).collect();
        let mut d = Diagnostic::error(span, format!("unknown variable `{name}`"));
        if let Some(m) = did_you_mean(name, refs) {
            d = d.with_help(format!("did you mean `{m}`?"));
        }
        self.diags.push(d);
    }

    fn resolvable(&self, name: &str, scope: &Scope) -> bool {
        if scope.iter().any(|f| f.contains_key(name)) {
            return true;
        }
        // Uppercase-initial names are types / constructors, not variables.
        if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            return true;
        }
        self.endpoints.contains(name)
            || self.flows.contains(name)
            || self.type_names.contains(name)
            || CONSTANTS.contains(&name)
            || BUILTINS.contains(&name)
    }

    fn bind(&self, scope: &mut Scope, name: &str, ty: Type) {
        scope
            .last_mut()
            .expect("non-empty scope")
            .insert(name.to_string(), ty);
    }

    fn lookup(&self, scope: &Scope, name: &str) -> Option<Type> {
        scope.iter().rev().find_map(|f| f.get(name).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diags(src: &str) -> Diagnostics {
        let (prog, pdiags) = tired_syntax::parse(src);
        assert!(
            !pdiags.has_errors(),
            "parse error:\n{}",
            pdiags.render(src, "t")
        );
        let (_an, d) = check(&prog);
        d
    }

    fn errs(src: &str) -> Vec<String> {
        diags(src)
            .items()
            .iter()
            .map(|d| d.message.clone())
            .collect()
    }

    #[test]
    fn duplicate_field_is_an_error() {
        let e = errs("type Repo { id: Integer  name: String  id: Float }");
        assert!(
            e.iter().any(|m| m.contains("duplicate field `id`")),
            "{e:?}"
        );
    }

    #[test]
    fn unknown_endpoint_is_an_error() {
        let e = errs("fetch GitGub /users/gabriel -> user");
        assert!(e.iter().any(|m| m.contains("unknown endpoint")), "{e:?}");
    }

    #[test]
    fn unknown_field_suggests_correction() {
        let src = r#"
            type Repo { stars: Integer name: String }
            endpoint GH { base: "x" }
            fetch GH /repos | filter(r => r.starz > 1) -> repos: Repo[]
        "#;
        let e = errs(src);
        assert!(e.iter().any(|m| m.contains("no field `starz`")), "{e:?}");
    }

    #[test]
    fn good_field_access_is_ok() {
        let src = r#"
            type Repo { stars: Integer name: String }
            endpoint GH { base: "x" }
            fetch GH /repos | filter(r => r.stars > 1) -> repos: Repo[]
        "#;
        assert!(!diags(src).has_errors(), "{:?}", errs(src));
    }

    #[test]
    fn unhandled_result_is_an_error() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /charges/1 -> c: Result<Charge, ApiError>
            log "done"
        "#;
        let e = errs(src);
        assert!(e.iter().any(|m| m.contains("unhandled error")), "{e:?}");
    }

    #[test]
    fn non_exhaustive_match_is_an_error() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /charges/1 -> c: Result<Charge, ApiError>
            match c {
                Ok(charge) => charge
                Err(NotFound) => other()
            }
        "#;
        let e = errs(src);
        assert!(e.iter().any(|m| m.contains("non-exhaustive")), "{e:?}");
    }

    #[test]
    fn exhaustive_match_with_catch_all_is_ok() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /charges/1 -> c: Result<Charge, ApiError>
            match c {
                Ok(charge) => charge
                Err(e) => fallback()
            }
        "#;
        assert!(!diags(src).has_errors(), "{:?}", errs(src));
    }

    #[test]
    fn closed_union_requires_each_variant() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /repos/1 -> r: Result<Repo, NotFound | Unauthorized>
            match r {
                Ok(repo) => repo
                Err(NotFound) => a()
            }
        "#;
        let e = errs(src);
        assert!(e.iter().any(|m| m.contains("Unauthorized")), "{e:?}");
    }

    #[test]
    fn field_access_on_result_is_rejected() {
        let src = r#"
            endpoint GH { base: "x" }
            fetch GH /charges/1 -> c: Result<Charge, ApiError>
            log c.amount
        "#;
        let e = errs(src);
        assert!(e.iter().any(|m| m.contains("cannot read field")), "{e:?}");
    }
}
