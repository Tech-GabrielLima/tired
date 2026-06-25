//! Runtime contract verification. When a fetch binding is annotated with a record type
//! whose fields carry `where (...)` constraints, the response is checked against them.
//! Only declared constraints "bite" — unknown/extra fields are tolerated, matching how
//! real APIs evolve. A violation aborts with a precise message.

use tired_compiler::types::TypeTable;
use tired_syntax::ast::{BinOp, Constraint, ConstraintSubject, TypeExpr};

use crate::eval::{eval, Env};
use crate::value::{ErrValue, Outcome, Value};

/// Validate `value` against the declared type `ty`. Returns `Err(message)` on the first
/// violation. Arrays are validated element-wise; `Result` validates its `Ok` payload.
pub fn validate(value: &Value, ty: &TypeExpr, table: &TypeTable) -> Result<(), String> {
    match ty {
        TypeExpr::Named(name) => validate_named(value, name, table),
        TypeExpr::Optional(inner) => {
            if matches!(value, Value::Null) {
                Ok(())
            } else {
                validate(value, inner, table)
            }
        }
        TypeExpr::Array(inner) => {
            if let Value::Array(items) = value {
                for it in items {
                    validate(it, inner, table)?;
                }
            }
            Ok(())
        }
        TypeExpr::Generic(name, args) if name == "Result" && args.len() == 2 => {
            // Only the success payload is contract-checked.
            if let Value::Ok(inner) = value {
                validate(inner, &args[0], table)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate the outcome of a fetch: only successes are checked.
pub fn validate_outcome(outcome: &Outcome, ty: &TypeExpr, table: &TypeTable) -> Result<(), String> {
    match outcome {
        Outcome::Success(v) => validate(v, ty, table),
        Outcome::Failure(_) => Ok(()),
    }
}

fn validate_named(value: &Value, name: &str, table: &TypeTable) -> Result<(), String> {
    let Some(fields) = table.fields(name) else {
        return Ok(()); // primitive or unknown type: nothing declared to check
    };
    for field in fields {
        let fv = value.get_field(&field.name);
        let nullable = matches!(field.decl.ty, TypeExpr::Optional(_));
        if matches!(fv, Value::Null) {
            if nullable {
                continue;
            }
            // A required field that is absent only fails if it carries a constraint;
            // otherwise we stay lenient about partial responses.
            if field.decl.constraint.is_some() {
                return Err(format!(
                    "contract `{name}`: required field `{}` is missing",
                    field.name
                ));
            }
            continue;
        }
        if let Some(c) = &field.decl.constraint {
            check_constraint(&fv, c, name, &field.name)?;
        }
    }
    Ok(())
}

fn check_constraint(value: &Value, c: &Constraint, ty: &str, field: &str) -> Result<(), String> {
    let env = Env::new();
    match c {
        Constraint::Cmp { subject, op, rhs } => {
            let lhs = subject_value(value, subject);
            let rv = eval(rhs, &env, None).map_err(|e| e.message)?;
            if !compare_ok(&lhs, *op, &rv) {
                return Err(format!(
                    "contract `{ty}`: field `{field}` violates `{} {} {}`",
                    subject_name(subject),
                    op.symbol(),
                    rv.display()
                ));
            }
        }
        Constraint::InRange { subject, lo, hi } => {
            let lhs = subject_value(value, subject);
            let lo = eval(lo, &env, None).map_err(|e| e.message)?;
            let hi = eval(hi, &env, None).map_err(|e| e.message)?;
            let ok = compare_ok(&lhs, BinOp::Ge, &lo) && compare_ok(&lhs, BinOp::Le, &hi);
            if !ok {
                return Err(format!(
                    "contract `{ty}`: field `{field}` ({}) is outside {}..{}",
                    lhs.display(),
                    lo.display(),
                    hi.display()
                ));
            }
        }
    }
    Ok(())
}

fn subject_value(value: &Value, subject: &ConstraintSubject) -> Value {
    match subject {
        ConstraintSubject::Value => value.clone(),
        ConstraintSubject::Length => value.get_field("length"),
    }
}

fn subject_name(subject: &ConstraintSubject) -> &'static str {
    match subject {
        ConstraintSubject::Value => "value",
        ConstraintSubject::Length => "length",
    }
}

fn compare_ok(lhs: &Value, op: BinOp, rhs: &Value) -> bool {
    use std::cmp::Ordering;
    let ord = match (lhs.as_number(), rhs.as_number()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => match (lhs, rhs) {
            (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
            _ => None,
        },
    };
    match op {
        BinOp::Eq => lhs == rhs,
        BinOp::Ne => lhs != rhs,
        BinOp::Lt => ord == Some(Ordering::Less),
        BinOp::Le => matches!(ord, Some(Ordering::Less) | Some(Ordering::Equal)),
        BinOp::Gt => ord == Some(Ordering::Greater),
        BinOp::Ge => matches!(ord, Some(Ordering::Greater) | Some(Ordering::Equal)),
        _ => true,
    }
}

// Re-exported for the executor to build failure values from contract errors.
pub fn contract_failure(msg: String) -> ErrValue {
    ErrValue::new("ContractViolation", None, msg)
}
