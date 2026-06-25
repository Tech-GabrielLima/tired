//! `tired schema` — export the `type`/`contract` declarations of a program as a
//! standard **JSON Schema** (2020-12). Field types map to JSON Schema types (with
//! semantic `format`s for `Url`/`Email`/`UUID`/`DateTime`), and `where (...)`
//! constraints become `minimum`/`maximum`/`minLength`/… keywords. This makes a TIRED
//! contract shareable with any tool that speaks JSON Schema.

use serde_json::{json, Map, Value as Json};
use tired_syntax::ast::{
    BinOp, Constraint, ConstraintSubject, Expr, Item, Program, TypeDecl, TypeExpr,
};

/// Build a JSON Schema document for all declared types. Returns `None` if there are none.
pub fn to_json_schema(program: &Program, title: &str) -> Option<String> {
    let types: Vec<&TypeDecl> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Type(t) => Some(t),
            _ => None,
        })
        .collect();
    if types.is_empty() {
        return None;
    }

    let mut defs = Map::new();
    for t in &types {
        defs.insert(t.name.node.clone(), type_object(t));
    }

    let doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": title,
        "$defs": defs,
    });
    serde_json::to_string_pretty(&doc).ok()
}

fn type_object(t: &TypeDecl) -> Json {
    let mut props = Map::new();
    let mut required = Vec::new();
    for f in &t.fields {
        let mut schema = type_expr_schema(&f.ty);
        if let (Some(obj), Some(c)) = (schema.as_object_mut(), &f.constraint) {
            apply_constraint(obj, c);
        }
        props.insert(f.name.node.clone(), schema);
        if !matches!(f.ty, TypeExpr::Optional(_)) {
            required.push(Json::String(f.name.node.clone()));
        }
    }
    json!({
        "type": "object",
        "title": t.name.node,
        "properties": props,
        "required": required,
        "additionalProperties": true,
    })
}

fn type_expr_schema(te: &TypeExpr) -> Json {
    match te {
        TypeExpr::Named(n) => named_schema(n),
        TypeExpr::Optional(inner) => type_expr_schema(inner),
        TypeExpr::Array(inner) => json!({ "type": "array", "items": type_expr_schema(inner) }),
        TypeExpr::Generic(name, args) if name == "Result" && args.len() == 2 => {
            type_expr_schema(&args[0])
        }
        TypeExpr::Generic(name, args) if name == "Option" && args.len() == 1 => {
            type_expr_schema(&args[0])
        }
        _ => json!({}),
    }
}

fn named_schema(n: &str) -> Json {
    match n {
        "Int" | "Integer" => json!({ "type": "integer" }),
        "Float" | "Number" => json!({ "type": "number" }),
        "Bool" | "Boolean" => json!({ "type": "boolean" }),
        "String" | "Str" => json!({ "type": "string" }),
        "Null" => json!({ "type": "null" }),
        "Url" => json!({ "type": "string", "format": "uri" }),
        "Email" => json!({ "type": "string", "format": "email" }),
        "UUID" | "Uuid" => json!({ "type": "string", "format": "uuid" }),
        "DateTime" | "Time" => json!({ "type": "string", "format": "date-time" }),
        "Date" => json!({ "type": "string", "format": "date" }),
        // Anything else is a reference to another declared type.
        _ => json!({ "$ref": format!("#/$defs/{n}") }),
    }
}

fn apply_constraint(obj: &mut Map<String, Json>, c: &Constraint) {
    match c {
        Constraint::Cmp { subject, op, rhs } => {
            if let Some(num) = literal_num(rhs) {
                let (lo_key, hi_key) = bound_keys(subject);
                match op {
                    BinOp::Ge => obj.insert(lo_key.into(), num),
                    BinOp::Gt => obj.insert(excl(lo_key).into(), num),
                    BinOp::Le => obj.insert(hi_key.into(), num),
                    BinOp::Lt => obj.insert(excl(hi_key).into(), num),
                    BinOp::Eq => obj.insert("const".into(), num),
                    _ => None,
                };
            }
        }
        Constraint::InRange { subject, lo, hi } => {
            let (lo_key, hi_key) = bound_keys(subject);
            if let Some(n) = literal_num(lo) {
                obj.insert(lo_key.into(), n);
            }
            if let Some(n) = literal_num(hi) {
                obj.insert(hi_key.into(), n);
            }
        }
    }
}

/// (minimum-ish key, maximum-ish key) depending on whether the constraint is on the
/// value or its length.
fn bound_keys(subject: &ConstraintSubject) -> (&'static str, &'static str) {
    match subject {
        ConstraintSubject::Value => ("minimum", "maximum"),
        ConstraintSubject::Length => ("minLength", "maxLength"),
    }
}

fn excl(key: &str) -> &'static str {
    match key {
        "minimum" => "exclusiveMinimum",
        "maximum" => "exclusiveMaximum",
        "minLength" => "minLength",
        _ => "maxLength",
    }
}

fn literal_num(e: &Expr) -> Option<Json> {
    match e {
        Expr::Int(n, _) => Some(json!(n)),
        Expr::Float(f, _) => Some(json!(f)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_schema_with_constraints_and_refs() {
        let src = r#"
            type Owner { id: Integer  login: String }
            contract Repo {
                id:    Integer where (> 0)
                name:  String  where (length in 1..100)
                stars: Integer where (>= 0)
                owner: Owner
                site:  Url
            }
        "#;
        let (prog, _) = tired_syntax::parse(src);
        let out = to_json_schema(&prog, "API").unwrap();
        let v: Json = serde_json::from_str(&out).unwrap();
        let repo = &v["$defs"]["Repo"];
        assert_eq!(repo["properties"]["id"]["exclusiveMinimum"], json!(0));
        assert_eq!(repo["properties"]["name"]["minLength"], json!(1));
        assert_eq!(repo["properties"]["name"]["maxLength"], json!(100));
        assert_eq!(repo["properties"]["stars"]["minimum"], json!(0));
        assert_eq!(repo["properties"]["owner"]["$ref"], json!("#/$defs/Owner"));
        assert_eq!(repo["properties"]["site"]["format"], json!("uri"));
    }
}
