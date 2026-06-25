//! Synchronous expression evaluation: arithmetic, comparisons, field access, records,
//! arrays, string interpolation, the pipeline operators (`filter`/`map`/`sort`/`limit`)
//! and pattern matching. Anything that touches the network is a *statement* and lives
//! in [`crate::exec`]; everything here is pure and runs on a single thread.

use std::cmp::Ordering;
use std::collections::HashMap;

use tired_syntax::ast::*;

use crate::value::{ErrValue, EvalResult, RunError, Value};

/// A lexical environment: variable name → value.
pub type Env = HashMap<String, Value>;

/// Evaluate an expression. `elem` is the implicit pipeline element (for `.field`
/// shorthands and bare predicates).
pub fn eval(e: &Expr, env: &Env, elem: Option<&Value>) -> EvalResult {
    match e {
        Expr::Int(n, _) => Ok(Value::Int(*n)),
        Expr::Float(f, _) => Ok(Value::Float(*f)),
        Expr::Bool(b, _) => Ok(Value::Bool(*b)),
        Expr::Null(_) => Ok(Value::Null),
        Expr::Duration(d, _) => Ok(Value::Duration(*d)),
        Expr::Str { parts, .. } => {
            let mut s = String::new();
            for p in parts {
                match p {
                    StrPart::Lit(t) => s.push_str(t),
                    StrPart::Interp(ex) => s.push_str(&eval(ex, env, elem)?.display()),
                }
            }
            Ok(Value::Str(s))
        }
        Expr::Ident(n) => {
            if let Some(v) = env.get(&n.node) {
                Ok(v.clone())
            } else if n.node == "None" {
                Ok(Value::Null)
            } else {
                Err(RunError::new(format!("undefined variable `{}`", n.node)))
            }
        }
        Expr::EnvVar(n) => {
            // A mock injects captured path params here; otherwise read the process env.
            if let Some(v) = env.get(&n.node) {
                Ok(v.clone())
            } else {
                Ok(std::env::var(&n.node)
                    .map(Value::Str)
                    .unwrap_or(Value::Null))
            }
        }
        Expr::ImplicitField(field) => Ok(elem
            .map(|v| v.get_field(&field.node))
            .unwrap_or(Value::Null)),
        Expr::Field { base, field, .. } => {
            let b = eval(base, env, elem)?;
            Ok(b.get_field(&field.node))
        }
        Expr::Call { callee, args, .. } => eval_call(callee, args, env, elem),
        Expr::Unary { op, rhs, .. } => {
            let v = eval(rhs, env, elem)?;
            match op {
                UnOp::Not => Ok(Value::Bool(!v.truthy())),
                UnOp::Neg => match v {
                    Value::Int(n) => Ok(Value::Int(-n)),
                    Value::Float(f) => Ok(Value::Float(-f)),
                    _ => Err(RunError::new("cannot negate a non-number")),
                },
            }
        }
        Expr::Binary { op, lhs, rhs, .. } => eval_binary(*op, lhs, rhs, env, elem),
        Expr::Record { fields, .. } => {
            let mut o = std::collections::BTreeMap::new();
            for (k, v) in fields {
                o.insert(k.node.clone(), eval(v, env, elem)?);
            }
            Ok(Value::Object(o))
        }
        Expr::Array { elems, .. } => {
            let mut out = Vec::new();
            for x in elems {
                if let Expr::Spread { expr, .. } = x {
                    match eval(expr, env, elem)? {
                        Value::Array(items) => out.extend(items),
                        other => out.push(other),
                    }
                } else {
                    out.push(eval(x, env, elem)?);
                }
            }
            Ok(Value::Array(out))
        }
        Expr::Spread { expr, .. } => eval(expr, env, elem),
        Expr::Lambda { .. } => Err(RunError::new(
            "a lambda is only valid inside a pipeline operator",
        )),
        Expr::Match(m) => eval_match_sync(m, env, elem),
        Expr::Range { .. } => Err(RunError::new("a range is not a runtime value")),
    }
}

fn eval_binary(op: BinOp, lhs: &Expr, rhs: &Expr, env: &Env, elem: Option<&Value>) -> EvalResult {
    // Short-circuit logical operators.
    if matches!(op, BinOp::And) {
        return Ok(Value::Bool(
            eval(lhs, env, elem)?.truthy() && eval(rhs, env, elem)?.truthy(),
        ));
    }
    if matches!(op, BinOp::Or) {
        return Ok(Value::Bool(
            eval(lhs, env, elem)?.truthy() || eval(rhs, env, elem)?.truthy(),
        ));
    }
    let l = eval(lhs, env, elem)?;
    let r = eval(rhs, env, elem)?;
    match op {
        BinOp::Add => add(&l, &r),
        BinOp::Sub => arith(&l, &r, |a, b| a - b, |a, b| a - b),
        BinOp::Mul => arith(&l, &r, |a, b| a * b, |a, b| a * b),
        BinOp::Eq => Ok(Value::Bool(l == r)),
        BinOp::Ne => Ok(Value::Bool(l != r)),
        BinOp::Lt => cmp_bool(&l, &r, |o| o == Ordering::Less),
        BinOp::Le => cmp_bool(&l, &r, |o| o != Ordering::Greater),
        BinOp::Gt => cmp_bool(&l, &r, |o| o == Ordering::Greater),
        BinOp::Ge => cmp_bool(&l, &r, |o| o != Ordering::Less),
        BinOp::And | BinOp::Or => unreachable!(),
    }
}

fn add(l: &Value, r: &Value) -> EvalResult {
    match (l, r) {
        (Value::Str(a), b) => Ok(Value::Str(format!("{a}{}", b.display()))),
        (a, Value::Str(b)) => Ok(Value::Str(format!("{}{b}", a.display()))),
        _ => arith(l, r, |a, b| a + b, |a, b| a + b),
    }
}

fn arith(l: &Value, r: &Value, fi: fn(i64, i64) -> i64, ff: fn(f64, f64) -> f64) -> EvalResult {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(fi(*a, *b))),
        _ => match (l.as_number(), r.as_number()) {
            (Some(a), Some(b)) => Ok(Value::Float(ff(a, b))),
            _ => Err(RunError::new("arithmetic requires numbers")),
        },
    }
}

fn cmp_bool(l: &Value, r: &Value, pred: fn(Ordering) -> bool) -> EvalResult {
    match compare(l, r) {
        Some(o) => Ok(Value::Bool(pred(o))),
        None => Err(RunError::new("values are not comparable")),
    }
}

fn compare(l: &Value, r: &Value) -> Option<Ordering> {
    match (l.as_number(), r.as_number()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => match (l, r) {
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            _ => None,
        },
    }
}

fn eval_call(callee: &Expr, args: &[Expr], env: &Env, elem: Option<&Value>) -> EvalResult {
    match callee {
        Expr::Ident(n) => match n.node.as_str() {
            "Ok" => Ok(Value::Ok(Box::new(eval_arg(args, 0, env, elem)?))),
            "Some" => Ok(eval_arg(args, 0, env, elem)?),
            "None" => Ok(Value::Null),
            "Err" => Ok(Value::Err(build_err(args.first(), env, elem)?)),
            "length" => Ok(eval_arg(args, 0, env, elem)?.get_field("length")),
            "uuid" => Ok(Value::Str("00000000-0000-4000-8000-000000000000".into())),
            "now" => Ok(Value::Str("1970-01-01T00:00:00Z".into())),
            // Open-world external functions (e.g. `default_charge()`): no-ops here.
            _ => Ok(Value::Null),
        },
        Expr::Field { base, field, .. } => eval_method(base, &field.node, args, env, elem),
        _ => Ok(Value::Null),
    }
}

fn eval_arg(args: &[Expr], i: usize, env: &Env, elem: Option<&Value>) -> EvalResult {
    match args.get(i) {
        Some(e) => eval(e, env, elem),
        None => Ok(Value::Null),
    }
}

fn build_err(arg: Option<&Expr>, env: &Env, elem: Option<&Value>) -> Result<ErrValue, RunError> {
    match arg {
        None => Ok(ErrValue::new("Error", None, "Error")),
        Some(Expr::Ident(n)) => Ok(ErrValue::new(n.node.clone(), None, n.node.clone())),
        Some(Expr::Call { callee, args, .. }) => {
            let variant = match callee.as_ref() {
                Expr::Ident(n) => n.node.clone(),
                _ => "Error".into(),
            };
            let payload = eval_arg(args, 0, env, elem)?;
            Ok(ErrValue::new(variant.clone(), None, variant).with_payload(payload))
        }
        Some(other) => Ok(ErrValue::new(
            "Error",
            None,
            eval(other, env, elem)?.display(),
        )),
    }
}

fn eval_method(
    base: &Expr,
    method: &str,
    args: &[Expr],
    env: &Env,
    elem: Option<&Value>,
) -> EvalResult {
    // `Type.fake(n)` generates n synthetic records (used by mocks).
    if method == "fake" {
        let n = match eval_arg(args, 0, env, elem)? {
            Value::Int(n) => n.max(0) as usize,
            _ => 1,
        };
        let mut out = Vec::new();
        for i in 0..n {
            let mut o = std::collections::BTreeMap::new();
            o.insert("id".to_string(), Value::Int(i as i64 + 1));
            o.insert("name".to_string(), Value::Str(format!("item-{}", i + 1)));
            out.push(Value::Object(o));
        }
        return Ok(Value::Array(out));
    }

    let recv = eval(base, env, elem)?;
    match method {
        "length" => Ok(recv.get_field("length")),
        "all" | "any" | "none" => {
            let items = as_array(&recv);
            let lam = args.first();
            let mut results = Vec::new();
            for it in &items {
                results.push(apply_predicate(lam, env, it)?);
            }
            let v = match method {
                "all" => results.iter().all(|b| *b),
                "any" => results.iter().any(|b| *b),
                _ => !results.iter().any(|b| *b),
            };
            Ok(Value::Bool(v))
        }
        "first" => Ok(as_array(&recv).first().cloned().unwrap_or(Value::Null)),
        "last" => Ok(as_array(&recv).last().cloned().unwrap_or(Value::Null)),
        _ => Ok(Value::Null),
    }
}

fn as_array(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// Apply a pipeline predicate (lambda or bare implicit-field expr) to one element.
fn apply_predicate(lambda: Option<&Expr>, env: &Env, item: &Value) -> Result<bool, RunError> {
    Ok(apply_lambda(lambda, env, item)?.truthy())
}

fn apply_lambda(lambda: Option<&Expr>, env: &Env, item: &Value) -> EvalResult {
    match lambda {
        Some(Expr::Lambda { param, body, .. }) => {
            let mut child = env.clone();
            child.insert(param.node.clone(), item.clone());
            eval(body, &child, Some(item))
        }
        Some(other) => eval(other, env, Some(item)),
        None => Ok(item.clone()),
    }
}

/// Apply a `filter | map | sort | limit` pipeline to a value (typically an array).
pub fn apply_pipeline(mut value: Value, ops: &[PipelineOp], env: &Env) -> EvalResult {
    for op in ops {
        value = match op {
            PipelineOp::Filter { lambda, .. } => {
                let mut out = Vec::new();
                for it in as_array(&value) {
                    if apply_predicate(Some(lambda), env, &it)? {
                        out.push(it);
                    }
                }
                Value::Array(out)
            }
            PipelineOp::Map { lambda, .. } => {
                let mut out = Vec::new();
                for it in as_array(&value) {
                    out.push(apply_lambda(Some(lambda), env, &it)?);
                }
                Value::Array(out)
            }
            PipelineOp::Sort { by, desc, .. } => {
                let mut items = as_array(&value);
                let mut keyed: Vec<(Value, Value)> = Vec::new();
                for it in items.drain(..) {
                    let key = eval(by, env, Some(&it))?;
                    keyed.push((key, it));
                }
                keyed.sort_by(|a, b| {
                    let ord = compare(&a.0, &b.0).unwrap_or(Ordering::Equal);
                    if *desc {
                        ord.reverse()
                    } else {
                        ord
                    }
                });
                Value::Array(keyed.into_iter().map(|(_, v)| v).collect())
            }
            PipelineOp::Limit { count, .. } => {
                let n = match eval(count, env, None)? {
                    Value::Int(n) => n.max(0) as usize,
                    _ => return Err(RunError::new("limit() expects an integer")),
                };
                let mut items = as_array(&value);
                items.truncate(n);
                Value::Array(items)
            }
        };
    }
    Ok(value)
}

// ---------- pattern matching ----------

/// Try to match `value` against `pat`, returning the captured bindings on success.
pub fn match_pattern(pat: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
    match pat {
        Pattern::Wildcard(_) => Some(Vec::new()),
        Pattern::Binding(n) => Some(vec![(n.node.clone(), value.clone())]),
        Pattern::Ctor { name, args, .. } => match name.node.as_str() {
            "Ok" => match value {
                Value::Ok(inner) => match_pattern(args.first()?, inner),
                _ => None,
            },
            "Err" => match value {
                Value::Err(e) => match args.first() {
                    None => Some(Vec::new()),
                    Some(Pattern::Wildcard(_)) => Some(Vec::new()),
                    Some(Pattern::Binding(b)) => Some(vec![(b.node.clone(), value.clone())]),
                    Some(Pattern::Ctor {
                        name: v,
                        args: inner,
                        ..
                    }) => {
                        if v.node != e.variant {
                            return None;
                        }
                        match inner.first() {
                            None => Some(Vec::new()),
                            Some(p) => {
                                let payload = e
                                    .payload
                                    .as_deref()
                                    .cloned()
                                    .or_else(|| e.status.map(|s| Value::Int(s as i64)))
                                    .unwrap_or(Value::Null);
                                match_pattern(p, &payload)
                            }
                        }
                    }
                },
                _ => None,
            },
            "Some" => match value {
                Value::Null => None,
                other => match_pattern(args.first()?, other),
            },
            "None" => matches!(value, Value::Null).then(Vec::new),
            _ => None,
        },
    }
}

/// A restricted, synchronous `match` used when a match appears in expression position
/// (e.g. `let x = match r { ... }`). Arm bodies may not perform fetches.
fn eval_match_sync(m: &MatchExpr, env: &Env, elem: Option<&Value>) -> EvalResult {
    let scrut = eval(&m.scrutinee, env, elem)?;
    for arm in &m.arms {
        if let Some(binds) = match_pattern(&arm.pattern, &scrut) {
            let mut child = env.clone();
            for (k, v) in binds {
                child.insert(k, v);
            }
            return match &arm.body {
                ArmBody::Value(e) => eval(e, &child, elem),
                ArmBody::Block(b) => run_block_sync(&b.stmts, &mut child),
                ArmBody::Retry { .. } => Err(RunError::new(
                    "`retry` is only valid in a statement-level match",
                )),
            };
        }
    }
    Err(RunError::new("no match arm applied"))
}

fn run_block_sync(stmts: &[Stmt], env: &mut Env) -> EvalResult {
    let mut last = Value::Null;
    for s in stmts {
        match s {
            Stmt::Let { name, value, .. } => {
                let v = eval(value, env, None)?;
                env.insert(name.node.clone(), v);
            }
            Stmt::Log { value, .. } => {
                println!("{}", eval(value, env, None)?.display());
            }
            Stmt::Return { value, .. } => {
                return value
                    .as_ref()
                    .map(|v| eval(v, env, None))
                    .unwrap_or(Ok(Value::Null));
            }
            Stmt::Expr { expr, .. } => {
                last = eval(expr, env, None)?;
            }
            Stmt::Assert { value, .. } => {
                if !eval(value, env, None)?.truthy() {
                    return Err(RunError::new("assertion failed"));
                }
            }
            Stmt::Fetch(_) | Stmt::Parallel { .. } => {
                return Err(RunError::new(
                    "a fetch inside an expression-position match arm is not supported; lift it to a statement",
                ));
            }
            Stmt::UsingMock { .. } => {}
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tired_syntax::ast::Expr;

    fn parse_expr(src: &str) -> Expr {
        // Wrap as a `let` so the parser yields an expression we can pull back out.
        let (prog, d) = tired_syntax::parse(&format!("let __x = {src}"));
        assert!(!d.has_errors(), "{}", d.render(src, "t"));
        match &prog.items[0] {
            tired_syntax::ast::Item::Stmt(Stmt::Let { value, .. }) => value.clone(),
            _ => panic!("expected let"),
        }
    }

    fn run(src: &str) -> Value {
        eval(&parse_expr(src), &Env::new(), None).unwrap()
    }

    #[test]
    fn arithmetic_and_comparison() {
        assert_eq!(run("1 + 2 * 3"), Value::Int(7));
        assert_eq!(run("10 > 3 and 2 == 2"), Value::Bool(true));
        assert_eq!(run(r#""a" + "b""#), Value::Str("ab".into()));
    }

    #[test]
    fn ok_err_construction_and_match() {
        let v = run("Err(NotFound)");
        match v {
            Value::Err(e) => assert_eq!(e.variant, "NotFound"),
            _ => panic!(),
        }
    }

    #[test]
    fn pattern_match_binds_payload() {
        let val =
            Value::Err(ErrValue::new("RateLimit", Some(429), "x").with_payload(Value::Int(1500)));
        let (prog, _) =
            tired_syntax::parse("flow F() { match e { Err(RateLimit(ms)) => ms _ => other() } }");
        // Reach into the parsed match for its first arm pattern.
        if let tired_syntax::ast::Item::Flow(f) = &prog.items[0] {
            if let Stmt::Expr {
                expr: Expr::Match(m),
                ..
            } = &f.body.stmts[0]
            {
                let binds = match_pattern(&m.arms[0].pattern, &val).unwrap();
                assert_eq!(binds, vec![("ms".to_string(), Value::Int(1500))]);
                return;
            }
        }
        panic!("did not find match");
    }
}
