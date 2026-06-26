//! Schema inference for `hale inspect`: given a JSON sample (from a live endpoint or a
//! file), reconstruct hale `type` declarations. Object fields become typed fields,
//! arrays of objects become `Elem[]` with a merged element type (fields seen in only
//! some elements are marked nullable), and strings get semantic types (`Url`, `Email`,
//! `DateTime`, `UUID`) by light heuristics.

use serde_json::{Map, Value};

/// Infer one or more `type` declarations describing `root`, named `root_name`.
pub fn infer_types(root: &Value, root_name: &str) -> String {
    let mut decls: Vec<(String, String)> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let top = type_of(root, root_name, &mut decls, &mut used);
    let mut out = String::new();
    for (name, body) in &decls {
        out.push_str(&format!("type {name} {{\n{body}}}\n\n"));
    }
    if decls.iter().all(|(n, _)| !top.starts_with(n)) && decls.is_empty() {
        out.push_str(&format!("// root value has type: {top}\n"));
    } else {
        out.push_str(&format!("// root: {top}\n"));
    }
    out
}

fn type_of(
    v: &Value,
    name: &str,
    decls: &mut Vec<(String, String)>,
    used: &mut Vec<String>,
) -> String {
    match v {
        Value::Null => "String?".into(),
        Value::Bool(_) => "Boolean".into(),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "Integer".into()
            } else {
                "Float".into()
            }
        }
        Value::String(s) => scalar_string_type(s).into(),
        Value::Array(items) => {
            let objs: Vec<&Map<String, Value>> =
                items.iter().filter_map(|x| x.as_object()).collect();
            if !objs.is_empty() && objs.len() == items.len() {
                let elem = object_type(&objs, &singular(name), decls, used);
                format!("{elem}[]")
            } else if let Some(first) = items.iter().find(|x| !x.is_null()) {
                format!("{}[]", type_of(first, &singular(name), decls, used))
            } else {
                "String[]".into()
            }
        }
        Value::Object(map) => object_type(&[map], name, decls, used),
    }
}

fn object_type(
    samples: &[&Map<String, Value>],
    name: &str,
    decls: &mut Vec<(String, String)>,
    used: &mut Vec<String>,
) -> String {
    let type_name = unique_name(&capitalize(name), used);

    // Ordered union of keys across all samples.
    let mut keys: Vec<String> = Vec::new();
    for s in samples {
        for k in s.keys() {
            if !keys.contains(k) {
                keys.push(k.clone());
            }
        }
    }

    let total = samples.len();
    let mut body = String::new();
    for key in &keys {
        let present: Vec<&Value> = samples.iter().filter_map(|s| s.get(key)).collect();
        let any_null = present.iter().any(|v| v.is_null());
        let missing = present.len() < total;
        let nullable = any_null || missing;
        let base = present
            .iter()
            .find(|v| !v.is_null())
            .map(|v| type_of(v, key, decls, used))
            .unwrap_or_else(|| "String".into());
        let ty = if nullable && !base.ends_with('?') {
            format!("{base}?")
        } else {
            base
        };
        body.push_str(&format!("  {key}: {ty}\n"));
    }

    decls.push((type_name.clone(), body));
    type_name
}

fn scalar_string_type(s: &str) -> &'static str {
    if s.starts_with("http://") || s.starts_with("https://") {
        "Url"
    } else if is_email(s) {
        "Email"
    } else if is_uuid(s) {
        "UUID"
    } else if is_datetime(s) {
        "DateTime"
    } else {
        "String"
    }
}

fn is_email(s: &str) -> bool {
    let at = s.find('@');
    match at {
        Some(i) if i > 0 && i < s.len() - 1 => {
            let domain = &s[i + 1..];
            domain.contains('.') && !s.contains(' ') && s.matches('@').count() == 1
        }
        _ => false,
    }
}

fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() == 36
        && b[8] == b'-'
        && b[13] == b'-'
        && b[18] == b'-'
        && b[23] == b'-'
        && s.chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) || c.is_ascii_hexdigit())
}

fn is_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    s.len() >= 10
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[7] == b'-'
}

fn capitalize(s: &str) -> String {
    // Keep only identifier-ish chars; capitalize first letter.
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    let cleaned = if cleaned.is_empty() {
        "Type".to_string()
    } else {
        cleaned
    };
    let mut chars = cleaned.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => cleaned,
    }
}

fn singular(name: &str) -> String {
    if let Some(stem) = name.strip_suffix("ies") {
        format!("{stem}y")
    } else if name.ends_with("ss") {
        name.to_string()
    } else if let Some(stem) = name.strip_suffix('s') {
        stem.to_string()
    } else {
        name.to_string()
    }
}

fn unique_name(base: &str, used: &mut Vec<String>) -> String {
    if !used.contains(&base.to_string()) {
        used.push(base.to_string());
        return base.to_string();
    }
    let mut i = 2;
    loop {
        let candidate = format!("{base}{i}");
        if !used.contains(&candidate) {
            used.push(candidate.clone());
            return candidate;
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_object_with_semantics_and_nested() {
        let json: Value = serde_json::from_str(
            r#"{ "id": 7, "login": "octocat", "email": "o@x.com",
                "url": "https://x.com", "score": 1.5, "owner": { "id": 1, "name": "n" },
                "repos": [ { "id": 1, "stars": 10 }, { "id": 2 } ] }"#,
        )
        .unwrap();
        let out = infer_types(&json, "User");
        assert!(out.contains("type User"), "{out}");
        assert!(out.contains("id: Integer"), "{out}");
        assert!(out.contains("email: Email"), "{out}");
        assert!(out.contains("url: Url"), "{out}");
        assert!(out.contains("score: Float"), "{out}");
        assert!(out.contains("type Owner"), "{out}");
        assert!(out.contains("repos: Repo[]"), "{out}");
        // `stars` is present in only one repo element -> nullable.
        assert!(out.contains("stars: Integer?"), "{out}");
    }
}
