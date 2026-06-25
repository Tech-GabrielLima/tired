//! Record & replay — TIRED's "time-travel" mode. In **record** mode every request's raw
//! outcome is captured, keyed by `METHOD endpoint/path?query`, and written to a JSON
//! file. In **replay** mode the same file is served back instead of hitting the
//! network, so a run is fully deterministic and offline — ideal for debugging exactly
//! what an API returned, or reproducing a bug without the live service.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{Map, Value as Json};

use crate::value::{from_json, ErrValue, Outcome, Value};

pub enum RecordMode {
    Off,
    Record(Mutex<BTreeMap<String, Outcome>>),
    Replay(BTreeMap<String, Outcome>),
}

impl RecordMode {
    pub fn record() -> Self {
        RecordMode::Record(Mutex::new(BTreeMap::new()))
    }

    pub fn replay_from(path: &str) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read `{path}`: {e}"))?;
        let json: Json =
            serde_json::from_str(&text).map_err(|e| format!("invalid recording `{path}`: {e}"))?;
        let obj = json
            .as_object()
            .ok_or_else(|| "recording must be a JSON object".to_string())?;
        let mut map = BTreeMap::new();
        for (k, v) in obj {
            map.insert(k.clone(), outcome_from_json(v));
        }
        Ok(RecordMode::Replay(map))
    }

    /// In replay mode, look up a previously recorded outcome.
    pub fn lookup(&self, key: &str) -> Option<Outcome> {
        match self {
            RecordMode::Replay(map) => map.get(key).cloned(),
            _ => None,
        }
    }

    pub fn is_replay(&self) -> bool {
        matches!(self, RecordMode::Replay(_))
    }

    /// In record mode, remember an outcome.
    pub fn store(&self, key: String, outcome: &Outcome) {
        if let RecordMode::Record(mtx) = self {
            mtx.lock().unwrap().insert(key, outcome.clone());
        }
    }

    /// Serialize the captured outcomes (record mode) to a pretty JSON string.
    pub fn to_json_string(&self) -> Option<String> {
        if let RecordMode::Record(mtx) = self {
            let map = mtx.lock().unwrap();
            let mut obj = Map::new();
            for (k, v) in map.iter() {
                obj.insert(k.clone(), outcome_to_json(v));
            }
            serde_json::to_string_pretty(&Json::Object(obj)).ok()
        } else {
            None
        }
    }
}

/// Canonical key for a request: `METHOD endpoint/path?sortedquery`.
pub fn request_key(endpoint: &str, path: &str, query: &[(String, String)]) -> String {
    let mut q = query.to_vec();
    q.sort();
    let qs = if q.is_empty() {
        String::new()
    } else {
        let parts: Vec<String> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("?{}", parts.join("&"))
    };
    format!("GET {endpoint}{path}{qs}")
}

fn outcome_to_json(o: &Outcome) -> Json {
    match o {
        Outcome::Success(v) => Json::Object(Map::from_iter([("ok".to_string(), value_to_json(v))])),
        Outcome::Failure(e) => {
            let mut m = Map::new();
            m.insert("variant".into(), Json::String(e.variant.clone()));
            if let Some(s) = e.status {
                m.insert("status".into(), Json::from(s));
            }
            m.insert("message".into(), Json::String(e.message.clone()));
            Json::Object(Map::from_iter([("err".to_string(), Json::Object(m))]))
        }
    }
}

fn outcome_from_json(v: &Json) -> Outcome {
    if let Some(ok) = v.get("ok") {
        return Outcome::Success(from_json(ok));
    }
    if let Some(err) = v.get("err") {
        let variant = err
            .get("variant")
            .and_then(|x| x.as_str())
            .unwrap_or("Error")
            .to_string();
        let status = err.get("status").and_then(|x| x.as_u64()).map(|n| n as u16);
        let message = err
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        return Outcome::Failure(ErrValue::new(variant, status, message));
    }
    Outcome::Failure(ErrValue::new(
        "MalformedRecording",
        None,
        "expected `ok` or `err`",
    ))
}

/// Convert a runtime [`Value`] back into plain JSON for storage.
pub fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::Duration(d) => Json::from(*d),
        Value::Str(s) => Json::String(s.clone()),
        Value::Array(a) => Json::Array(a.iter().map(value_to_json).collect()),
        Value::Object(o) => Json::Object(
            o.iter()
                .map(|(k, val)| (k.clone(), value_to_json(val)))
                .collect(),
        ),
        Value::Ok(inner) => {
            Json::Object(Map::from_iter([("$ok".to_string(), value_to_json(inner))]))
        }
        Value::Err(e) => Json::Object(Map::from_iter([(
            "$err".to_string(),
            Json::String(e.variant.clone()),
        )])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_outcomes() {
        let key = request_key("GitHub", "/users/x", &[("a".into(), "1".into())]);
        assert_eq!(key, "GET GitHub/users/x?a=1");

        let rec = RecordMode::record();
        rec.store(
            key.clone(),
            &Outcome::Success(from_json(&serde_json::json!({ "id": 7 }))),
        );
        rec.store(
            "GET GitHub/missing".into(),
            &Outcome::Failure(ErrValue::new("NotFound", Some(404), "x")),
        );
        let json = rec.to_json_string().unwrap();

        // Write/read through the replay loader.
        let dir = std::env::temp_dir().join(format!("tired-rec-{}.json", std::process::id()));
        std::fs::write(&dir, &json).unwrap();
        let replay = RecordMode::replay_from(dir.to_str().unwrap()).unwrap();
        match replay.lookup(&key) {
            Some(Outcome::Success(v)) => assert_eq!(v.get_field("id"), Value::Int(7)),
            _ => panic!("expected recorded success"),
        }
        match replay.lookup("GET GitHub/missing") {
            Some(Outcome::Failure(e)) => assert_eq!(e.variant, "NotFound"),
            _ => panic!("expected recorded failure"),
        }
        let _ = std::fs::remove_file(dir);
    }
}
