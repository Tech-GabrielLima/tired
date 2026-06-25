//! A pretty-printer over the AST. Powers `tired fmt` (canonical formatting) and is also
//! used for short, readable expression snippets in runtime error messages.

use crate::ast::*;

/// Format an entire program into canonical TIRED source.
pub fn program(p: &Program) -> String {
    let mut out = String::new();
    for (i, item) in p.items.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format_item(item));
        out.push('\n');
    }
    out
}

fn format_item(item: &Item) -> String {
    match item {
        Item::Endpoint(e) => {
            let mut s = format!("endpoint {} {{\n", e.name.node);
            for setting in &e.settings {
                let vals: Vec<String> = setting.values.iter().map(expr).collect();
                s.push_str(&format!("  {}: {}\n", setting.key.node, vals.join(" ")));
            }
            s.push('}');
            s
        }
        Item::Type(t) => {
            let kw = if t.is_contract { "contract" } else { "type" };
            let mut s = format!("{kw} {} {{\n", t.name.node);
            for f in &t.fields {
                let c = match &f.constraint {
                    Some(c) => format!(" where ({})", constraint(c)),
                    None => String::new(),
                };
                s.push_str(&format!("  {}: {}{}\n", f.name.node, type_expr(&f.ty), c));
            }
            s.push('}');
            s
        }
        Item::Flow(f) => {
            let params: Vec<String> = f
                .params
                .iter()
                .map(|p| format!("{}: {}", p.name.node, type_expr(&p.ty)))
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|r| format!(" -> {}", type_expr(r)))
                .unwrap_or_default();
            let mut s = format!("flow {}({}){} {{\n", f.name.node, params.join(", "), ret);
            s.push_str(&block_body(&f.body, 1));
            s.push('}');
            s
        }
        Item::Mock(m) => {
            let mut s = format!("mock {} {{\n", m.name.node);
            for r in &m.routes {
                s.push_str(&format!(
                    "  {} {} -> {}\n",
                    r.method.node,
                    path(&r.path),
                    expr(&r.response)
                ));
            }
            s.push('}');
            s
        }
        Item::Test(t) => {
            let mut s = format!("test {:?} {{\n", t.description);
            s.push_str(&block_body(&t.body, 1));
            s.push('}');
            s
        }
        Item::Stmt(st) => stmt(st, 0),
    }
}

fn block_body(b: &Block, indent: usize) -> String {
    let mut s = String::new();
    for st in &b.stmts {
        s.push_str(&stmt(st, indent));
        s.push('\n');
    }
    s
}

fn pad(indent: usize) -> String {
    "  ".repeat(indent)
}

fn stmt(s: &Stmt, indent: usize) -> String {
    let p = pad(indent);
    match s {
        Stmt::Fetch(f) => {
            let mut out = format!("{p}fetch {} {}", f.endpoint.node, path(&f.path));
            if !f.params.is_empty() {
                let kvs: Vec<String> = f
                    .params
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.node, expr(v)))
                    .collect();
                out.push_str(&format!(" params {{ {} }}", kvs.join(", ")));
            }
            for op in &f.pipeline {
                out.push_str(&format!(" | {}", pipeline_op(op)));
            }
            if let Some(b) = &f.bind {
                out.push_str(&format!(" -> {}", binding(b)));
            }
            out
        }
        Stmt::Let { name, value, .. } => format!("{p}let {} = {}", name.node, expr(value)),
        Stmt::Log { value, .. } => format!("{p}log {}", expr(value)),
        Stmt::Parallel { block, .. } => {
            let mut out = format!("{p}parallel {{\n");
            out.push_str(&block_body(block, indent + 1));
            out.push_str(&format!("{p}}}"));
            out
        }
        Stmt::Return { value, .. } => match value {
            Some(v) => format!("{p}return {}", expr(v)),
            None => format!("{p}return"),
        },
        Stmt::Assert { value, .. } => format!("{p}assert {}", expr(value)),
        Stmt::UsingMock { name, .. } => format!("{p}using mock {}", name.node),
        Stmt::Expr { expr: e, bind, .. } => {
            let mut out = format!("{p}{}", expr_indented(e, indent));
            if let Some(b) = bind {
                out.push_str(&format!(" -> {}", binding(b)));
            }
            out
        }
    }
}

fn binding(b: &Binding) -> String {
    match &b.ty {
        Some(t) => format!("{}: {}", b.name.node, type_expr(t)),
        None => b.name.node.clone(),
    }
}

fn pipeline_op(op: &PipelineOp) -> String {
    match op {
        PipelineOp::Filter { lambda, .. } => format!("filter({})", expr(lambda)),
        PipelineOp::Map { lambda, .. } => format!("map({})", expr(lambda)),
        PipelineOp::Sort { by, desc, .. } => {
            format!(
                "sort(by: {} {})",
                expr(by),
                if *desc { "desc" } else { "asc" }
            )
        }
        PipelineOp::Limit { count, .. } => format!("limit({})", expr(count)),
    }
}

fn path(p: &PathPattern) -> String {
    let mut s = String::new();
    for seg in &p.segments {
        s.push('/');
        match seg {
            PathSeg::Literal(l) => s.push_str(l),
            PathSeg::Param(e) => s.push_str(&format!("{{{}}}", expr(e))),
        }
    }
    if s.is_empty() {
        s.push('/');
    }
    s
}

fn type_expr(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named(n) => n.clone(),
        TypeExpr::Optional(inner) => format!("{}?", type_expr(inner)),
        TypeExpr::Array(inner) => format!("{}[]", type_expr(inner)),
        TypeExpr::Generic(n, args) => {
            let a: Vec<String> = args.iter().map(type_expr).collect();
            format!("{n}<{}>", a.join(", "))
        }
        TypeExpr::Union(alts) => alts.iter().map(type_expr).collect::<Vec<_>>().join(" | "),
    }
}

fn constraint(c: &Constraint) -> String {
    match c {
        Constraint::Cmp { subject, op, rhs } => {
            format!("{} {} {}", subject_name(subject), op.symbol(), expr(rhs))
        }
        Constraint::InRange { subject, lo, hi } => {
            format!("{} in {}..{}", subject_name(subject), expr(lo), expr(hi))
        }
    }
}

fn subject_name(s: &ConstraintSubject) -> String {
    match s {
        ConstraintSubject::Value => String::new(),
        ConstraintSubject::Length => "length ".to_string(),
    }
    .trim_end()
    .to_string()
}

/// Operator precedence for minimal parenthesization.
fn prec(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul => 5,
    }
}

pub fn expr(e: &Expr) -> String {
    expr_indented(e, 0)
}

fn expr_indented(e: &Expr, indent: usize) -> String {
    match e {
        Expr::Int(n, _) => n.to_string(),
        Expr::Float(f, _) => f.to_string(),
        Expr::Bool(b, _) => b.to_string(),
        Expr::Null(_) => "null".into(),
        Expr::Duration(ms, _) => format!("{ms}ms"),
        Expr::Str { parts, .. } => string_lit(parts),
        Expr::Ident(n) => n.node.clone(),
        Expr::EnvVar(n) => format!("${}", n.node),
        Expr::ImplicitField(f) => format!(".{}", f.node),
        Expr::Field { base, field, .. } => format!("{}.{}", expr(base), field.node),
        Expr::Call { callee, args, .. } => {
            let a: Vec<String> = args.iter().map(expr).collect();
            format!("{}({})", expr(callee), a.join(", "))
        }
        Expr::Unary { op, rhs, .. } => match op {
            UnOp::Not => format!("not {}", expr(rhs)),
            UnOp::Neg => format!("-{}", expr(rhs)),
        },
        Expr::Binary { op, lhs, rhs, .. } => {
            let l = operand(lhs, prec(*op), false);
            let r = operand(rhs, prec(*op), true);
            format!("{l} {} {r}", op.symbol())
        }
        Expr::Record { name, fields, .. } => {
            let fs: Vec<String> = fields
                .iter()
                .map(|(k, v)| format!("{}: {}", k.node, expr(v)))
                .collect();
            let prefix = name
                .as_ref()
                .map(|n| format!("{} ", n.node))
                .unwrap_or_default();
            format!("{prefix}{{ {} }}", fs.join(", "))
        }
        Expr::Array { elems, .. } => {
            let es: Vec<String> = elems.iter().map(expr).collect();
            format!("[{}]", es.join(", "))
        }
        Expr::Spread { expr: inner, .. } => format!("...{}", expr(inner)),
        Expr::Lambda { param, body, .. } => format!("{} => {}", param.node, expr(body)),
        Expr::Range { lo, hi, .. } => format!("{}..{}", expr(lo), expr(hi)),
        Expr::Match(m) => {
            let p = pad(indent);
            let pi = pad(indent + 1);
            let mut s = format!("match {} {{\n", expr(&m.scrutinee));
            for arm in &m.arms {
                s.push_str(&format!(
                    "{pi}{} => {}\n",
                    pattern(&arm.pattern),
                    arm_body(&arm.body, indent + 1)
                ));
            }
            s.push_str(&format!("{p}}}"));
            s
        }
    }
}

fn operand(e: &Expr, parent_prec: u8, _right: bool) -> String {
    if let Expr::Binary { op, .. } = e {
        if prec(*op) < parent_prec {
            return format!("({})", expr(e));
        }
    }
    expr(e)
}

fn arm_body(b: &ArmBody, indent: usize) -> String {
    match b {
        ArmBody::Value(e) => expr_indented(e, indent),
        ArmBody::Block(block) => {
            let p = pad(indent);
            let mut s = "{\n".to_string();
            s.push_str(&block_body(block, indent + 1));
            s.push_str(&format!("{p}}}"));
            s
        }
        ArmBody::Retry { effects, .. } => {
            let mut parts = Vec::new();
            for eff in effects {
                match eff {
                    Effect::Call(e) => parts.push(expr(e)),
                    Effect::Wait(e) => parts.push(format!("wait({})", expr(e))),
                }
            }
            parts.push("retry".to_string());
            parts.join(" then ")
        }
    }
}

fn pattern(p: &Pattern) -> String {
    match p {
        Pattern::Wildcard(_) => "_".into(),
        Pattern::Binding(n) => n.node.clone(),
        Pattern::Ctor { name, args, .. } => {
            if args.is_empty() {
                name.node.clone()
            } else {
                let a: Vec<String> = args.iter().map(pattern).collect();
                format!("{}({})", name.node, a.join(", "))
            }
        }
    }
}

fn string_lit(parts: &[StrPart]) -> String {
    let mut s = String::from("\"");
    for p in parts {
        match p {
            StrPart::Lit(t) => {
                for c in t.chars() {
                    match c {
                        '"' => s.push_str("\\\""),
                        '\\' => s.push_str("\\\\"),
                        '\n' => s.push_str("\\n"),
                        '\t' => s.push_str("\\t"),
                        '{' => s.push_str("{{"),
                        '}' => s.push_str("}}"),
                        _ => s.push(c),
                    }
                }
            }
            StrPart::Interp(e) => {
                s.push('{');
                s.push_str(&expr(e));
                s.push('}');
            }
        }
    }
    s.push('"');
    s
}
