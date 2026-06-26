//! Runtime values. A [`Value`] is JSON plus the things hale adds: durations and the
//! `Ok`/`Err` results that make network-dependent typing real at runtime.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Duration(u64),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
    Ok(Box<Value>),
    Err(ErrValue),
}

/// A structured error result. `variant` is the symbolic name a `match` pattern binds
/// against (e.g. `NotFound`, `RateLimit`); `status` is the HTTP code when applicable.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrValue {
    pub variant: String,
    pub status: Option<u16>,
    pub message: String,
    /// Optional payload, e.g. the retry-after milliseconds for `RateLimit(ms)`.
    pub payload: Option<Box<Value>>,
}

impl ErrValue {
    pub fn new(
        variant: impl Into<String>,
        status: Option<u16>,
        message: impl Into<String>,
    ) -> Self {
        ErrValue {
            variant: variant.into(),
            status,
            message: message.into(),
            payload: None,
        }
    }
    pub fn with_payload(mut self, v: Value) -> Self {
        self.payload = Some(Box::new(v));
        self
    }
}

/// The result of a single request from any backend (HTTP or mock), before it is
/// wrapped into an `Ok`/`Err` value (for `Result`-typed fetches) or unwrapped (and an
/// error promoted to a [`RunError`]) for plain fetches.
#[derive(Clone, Debug)]
pub enum Outcome {
    Success(Value),
    Failure(ErrValue),
}

/// A runtime failure that aborts execution (distinct from a handled `Err` value).
#[derive(Clone, Debug)]
pub struct RunError {
    pub message: String,
}

impl RunError {
    pub fn new(message: impl Into<String>) -> Self {
        RunError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RunError {}

pub type EvalResult = Result<Value, RunError>;

impl Value {
    /// Map an HTTP status code to a result value: 2xx → `Ok(body)`, otherwise a typed
    /// `Err` whose variant is the conventional name for that class of status.
    pub fn from_status(status: u16, body: Value) -> Value {
        if (200..300).contains(&status) {
            return Value::Ok(Box::new(body));
        }
        let variant = match status {
            304 => "NotModified",
            400 => "BadRequest",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "NotFound",
            409 => "Conflict",
            429 => "RateLimit",
            408 | 504 => "Timeout",
            500..=599 => "ServerError",
            _ => "HttpError",
        };
        Value::Err(ErrValue::new(
            variant,
            Some(status),
            format!("HTTP {status}"),
        ))
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Null => false,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            Value::Ok(_) => true,
            Value::Err(_) => false,
            Value::Duration(d) => *d != 0,
        }
    }

    pub fn get_field(&self, name: &str) -> Value {
        match self {
            Value::Object(o) => o.get(name).cloned().unwrap_or(Value::Null),
            Value::Array(a) if name == "length" => Value::Int(a.len() as i64),
            Value::Str(s) if name == "length" => Value::Int(s.chars().count() as i64),
            _ => Value::Null,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(*f),
            Value::Duration(d) => Some(*d as f64),
            _ => None,
        }
    }

    /// A human-friendly rendering for `log`. Strings print bare; structures print JSON.
    pub fn display(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            other => other.to_json(),
        }
    }

    /// Strict JSON-ish serialization (used inside structures and for request bodies).
    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.write_json(&mut s);
        s
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(f) => {
                let _ = write!(out, "{f}");
            }
            Value::Duration(d) => {
                let _ = write!(out, "{d}ms");
            }
            Value::Str(s) => write_json_string(s, out),
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
            Value::Ok(v) => {
                out.push_str("Ok(");
                v.write_json(out);
                out.push(')');
            }
            Value::Err(e) => {
                let _ = write!(out, "Err({})", e.variant);
            }
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

/// Convert a parsed JSON document into a [`Value`].
pub fn from_json(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(a) => Value::Array(a.iter().map(from_json).collect()),
        serde_json::Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert!(matches!(Value::from_status(200, Value::Null), Value::Ok(_)));
        match Value::from_status(404, Value::Null) {
            Value::Err(e) => assert_eq!(e.variant, "NotFound"),
            _ => panic!(),
        }
        match Value::from_status(429, Value::Null) {
            Value::Err(e) => assert_eq!(e.variant, "RateLimit"),
            _ => panic!(),
        }
    }

    #[test]
    fn json_roundtrip_display() {
        let j: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":[true,"x"]}"#).unwrap();
        let v = from_json(&j);
        assert_eq!(v.get_field("a"), Value::Int(1));
        assert_eq!(v.get_field("b").get_field("length"), Value::Int(2));
    }
}
