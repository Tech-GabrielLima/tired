//! `tired-lsp` — a Language Server for TIRED over stdio (LSP / JSON-RPC).
//!
//! It reuses the real compiler: on every edit it runs `tired_compiler::analyze` and
//! publishes the resulting diagnostics (the exact "did you mean?", "unhandled error"
//! and dead-request messages you get on the CLI), plus keyword/endpoint **completion**
//! and **hover**. The message handling ([`Server::handle`]) is pure and unit-tested; the
//! [`run`] loop only adds stdio framing.

use std::collections::HashMap;
use std::io::{Read, Write};

use serde_json::{json, Value};
use tired_syntax::ast::{Item, StrPart};

const KEYWORDS: &[(&str, &str)] = &[
    (
        "endpoint",
        "Declare an HTTP endpoint (base URL, auth, retry, cache).",
    ),
    (
        "fetch",
        "Perform a GET request, optionally piping the result.",
    ),
    (
        "flow",
        "Define a reusable, parameterized sequence of operations.",
    ),
    ("type", "Declare a record type."),
    (
        "contract",
        "Declare a record type whose `where` constraints are checked at runtime.",
    ),
    ("mock", "Define an in-language mock for offline tests."),
    ("test", "A test block (runs against mocks)."),
    ("parallel", "Run the contained statements concurrently."),
    (
        "match",
        "Pattern-match on a value (exhaustive over Result).",
    ),
    ("let", "Bind a local value."),
    ("log", "Print a value."),
    ("return", "Return a value from a flow."),
    ("params", "Attach query parameters to a fetch."),
    ("filter", "Pipeline: keep elements matching a predicate."),
    ("map", "Pipeline: transform each element."),
    ("sort", "Pipeline: sort by a key (asc/desc)."),
    ("limit", "Pipeline: keep the first N elements."),
];

#[derive(Default)]
pub struct Server {
    docs: HashMap<String, String>,
    pub shutdown: bool,
}

impl Server {
    pub fn new() -> Self {
        Server::default()
    }

    /// Handle one JSON-RPC message, returning the messages to send back (responses and
    /// notifications). Pure: no IO, so it can be unit-tested directly.
    pub fn handle(&mut self, msg: &Value) -> Vec<Value> {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => vec![response(
                id,
                json!({
                    "capabilities": {
                        "textDocumentSync": 1,
                        "completionProvider": { "triggerCharacters": [" ", "/"] },
                        "hoverProvider": true
                    },
                    "serverInfo": { "name": "tired-lsp", "version": env!("CARGO_PKG_VERSION") }
                }),
            )],
            "initialized" => vec![],
            "textDocument/didOpen" => {
                let uri = str_at(&params, &["textDocument", "uri"]);
                let text = str_at(&params, &["textDocument", "text"]);
                if let Some(uri) = uri {
                    self.docs.insert(uri.clone(), text.unwrap_or_default());
                    return vec![self.diagnostics(&uri)];
                }
                vec![]
            }
            "textDocument/didChange" => {
                let uri = str_at(&params, &["textDocument", "uri"]);
                let text = params
                    .get("contentChanges")
                    .and_then(Value::as_array)
                    .and_then(|c| c.last())
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let (Some(uri), Some(text)) = (uri, text) {
                    self.docs.insert(uri.clone(), text);
                    return vec![self.diagnostics(&uri)];
                }
                vec![]
            }
            "textDocument/didClose" => {
                if let Some(uri) = str_at(&params, &["textDocument", "uri"]) {
                    self.docs.remove(&uri);
                }
                vec![]
            }
            "textDocument/completion" => {
                let uri = str_at(&params, &["textDocument", "uri"]).unwrap_or_default();
                vec![response(id, self.completions(&uri))]
            }
            "textDocument/hover" => {
                let uri = str_at(&params, &["textDocument", "uri"]).unwrap_or_default();
                let line = uint_at(&params, &["position", "line"]);
                let ch = uint_at(&params, &["position", "character"]);
                vec![response(id, self.hover(&uri, line, ch))]
            }
            "shutdown" => {
                self.shutdown = true;
                vec![response(id, Value::Null)]
            }
            "exit" => vec![],
            _ => {
                // Any other request must still get a response so the client doesn't hang.
                if id.is_some() {
                    vec![response(id, Value::Null)]
                } else {
                    vec![]
                }
            }
        }
    }

    fn diagnostics(&self, uri: &str) -> Value {
        let text = self.docs.get(uri).cloned().unwrap_or_default();
        let diags = tired_compiler::analyze(&text);
        let items: Vec<Value> = diags
            .items()
            .iter()
            .map(|d| {
                let (sl, sc) = offset_to_pos(&text, d.span.start);
                let (el, ec) = offset_to_pos(&text, d.span.end.max(d.span.start));
                let mut message = d.message.clone();
                if let Some(h) = &d.help {
                    message.push_str(&format!("\nhelp: {h}"));
                }
                if let Some(n) = &d.note {
                    message.push_str(&format!("\nnote: {n}"));
                }
                json!({
                    "range": { "start": { "line": sl, "character": sc }, "end": { "line": el, "character": ec } },
                    "severity": match d.severity { tired_syntax::Severity::Error => 1, _ => 2 },
                    "source": "tired",
                    "message": message
                })
            })
            .collect();
        notification(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": items }),
        )
    }

    fn completions(&self, uri: &str) -> Value {
        let text = self.docs.get(uri).cloned().unwrap_or_default();
        let (program, _) = tired_syntax::parse(&text);
        let mut items: Vec<Value> = Vec::new();

        for (kw, detail) in KEYWORDS {
            items.push(json!({ "label": kw, "kind": 14, "detail": *detail })); // 14 = Keyword
        }
        for item in &program.items {
            match item {
                Item::Endpoint(e) => {
                    items.push(json!({ "label": e.name.node, "kind": 9, "detail": "endpoint" }))
                } // 9 = Module
                Item::Flow(f) => {
                    items.push(json!({ "label": f.name.node, "kind": 3, "detail": "flow" }))
                } // 3 = Function
                Item::Type(t) => {
                    items.push(json!({ "label": t.name.node, "kind": 7, "detail": "type" }))
                } // 7 = Class
                _ => {}
            }
        }
        json!({ "isIncomplete": false, "items": items })
    }

    fn hover(&self, uri: &str, line: usize, character: usize) -> Value {
        let text = self.docs.get(uri).cloned().unwrap_or_default();
        let offset = pos_to_offset(&text, line, character);
        let word = word_at(&text, offset);
        if word.is_empty() {
            return Value::Null;
        }

        let (program, _) = tired_syntax::parse(&text);
        for item in &program.items {
            match item {
                Item::Endpoint(e) if e.name.node == word => {
                    let base = e
                        .settings
                        .iter()
                        .find(|s| s.key.node == "base")
                        .and_then(|s| s.values.first())
                        .map(string_lit)
                        .unwrap_or_default();
                    return md(&format!("**endpoint** `{}`\n\nbase: `{}`", word, base));
                }
                Item::Flow(f) if f.name.node == word => {
                    let ps: Vec<String> = f.params.iter().map(|p| p.name.node.clone()).collect();
                    return md(&format!("**flow** `{}({})`", word, ps.join(", ")));
                }
                Item::Type(t) if t.name.node == word => {
                    return md(&format!("**type** `{}` ({} fields)", word, t.fields.len()));
                }
                _ => {}
            }
        }
        if let Some((_, detail)) = KEYWORDS.iter().find(|(k, _)| *k == word) {
            return md(&format!("**{word}** — {detail}"));
        }
        Value::Null
    }
}

/// Run the stdio LSP loop until `exit`.
pub fn run() {
    let mut server = Server::new();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    while let Some(msg) = read_message(&mut reader) {
        let is_exit = msg.get("method").and_then(Value::as_str) == Some("exit");
        for out in server.handle(&msg) {
            write_message(&mut writer, &out);
        }
        if is_exit {
            break;
        }
    }
}

// ---------- JSON-RPC framing ----------

fn read_message(reader: &mut impl Read) -> Option<Value> {
    let mut content_length: Option<usize> = None;
    // Read headers line by line until a blank line.
    loop {
        let line = read_header_line(reader)?;
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn read_header_line(reader: &mut impl Read) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte).ok()? == 0 {
            return None;
        }
        if byte[0] == b'\n' {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Some(String::from_utf8_lossy(&line).into_owned());
        }
        line.push(byte[0]);
    }
}

fn write_message(writer: &mut impl Write, msg: &Value) {
    let body = serde_json::to_string(msg).unwrap_or_default();
    let _ = write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = writer.flush();
}

// ---------- helpers ----------

fn response(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn md(value: &str) -> Value {
    json!({ "contents": { "kind": "markdown", "value": value } })
}

fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_str().map(str::to_string)
}

fn uint_at(v: &Value, path: &[&str]) -> usize {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(next) => cur = next,
            None => return 0,
        }
    }
    cur.as_u64().unwrap_or(0) as usize
}

fn string_lit(e: &tired_syntax::ast::Expr) -> String {
    if let tired_syntax::ast::Expr::Str { parts, .. } = e {
        let mut s = String::new();
        for p in parts {
            if let StrPart::Lit(t) = p {
                s.push_str(t);
            }
        }
        s
    } else {
        String::new()
    }
}

fn offset_to_pos(text: &str, offset: usize) -> (usize, usize) {
    let (mut line, mut col, mut i) = (0usize, 0usize, 0usize);
    for ch in text.chars() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16();
        }
        i += ch.len_utf8();
    }
    (line, col)
}

fn pos_to_offset(text: &str, line: usize, character: usize) -> usize {
    let (mut cur_line, mut col, mut i) = (0usize, 0usize, 0usize);
    for ch in text.chars() {
        if cur_line == line && col >= character {
            return i;
        }
        if ch == '\n' {
            if cur_line == line {
                return i;
            }
            cur_line += 1;
            col = 0;
        } else {
            col += ch.len_utf16();
        }
        i += ch.len_utf8();
    }
    i
}

fn word_at(text: &str, offset: usize) -> String {
    let b = text.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    if b.is_empty() {
        return String::new();
    }
    let mut start = offset.min(b.len());
    let mut end = start;
    while start > 0 && is_word(b[start - 1]) {
        start -= 1;
    }
    while end < b.len() && is_word(b[end]) {
        end += 1;
    }
    text.get(start..end).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_diagnostics_on_open() {
        let mut s = Server::new();
        // unknown endpoint -> the compiler should flag it
        let open = json!({
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "file:///x.tired", "text": "fetch GitGub /u -> x\nlog \"hi\"" } }
        });
        let out = s.handle(&open);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["method"], "textDocument/publishDiagnostics");
        let diags = out[0]["params"]["diagnostics"].as_array().unwrap();
        assert!(
            diags
                .iter()
                .any(|d| d["message"].as_str().unwrap().contains("unknown endpoint")),
            "{diags:?}"
        );
        assert_eq!(diags[0]["severity"], 1); // error
    }

    #[test]
    fn completion_includes_keywords_and_endpoints() {
        let mut s = Server::new();
        s.handle(&json!({
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "u", "text": "endpoint GitHub { base: \"x\" }" } }
        }));
        let out = s.handle(&json!({
            "id": 1, "method": "textDocument/completion",
            "params": { "textDocument": { "uri": "u" } }
        }));
        let items = out[0]["result"]["items"].as_array().unwrap();
        let labels: Vec<&str> = items.iter().filter_map(|i| i["label"].as_str()).collect();
        assert!(labels.contains(&"fetch"));
        assert!(labels.contains(&"GitHub"));
    }

    #[test]
    fn hover_describes_endpoint() {
        let mut s = Server::new();
        let text = "endpoint GitHub { base: \"https://api.github.com\" }";
        s.handle(&json!({
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": "u", "text": text } }
        }));
        // hover over "GitHub" (line 0, char 9)
        let out = s.handle(&json!({
            "id": 2, "method": "textDocument/hover",
            "params": { "textDocument": { "uri": "u" }, "position": { "line": 0, "character": 11 } }
        }));
        let val = out[0]["result"]["contents"]["value"].as_str().unwrap_or("");
        assert!(
            val.contains("endpoint") && val.contains("api.github.com"),
            "{val}"
        );
    }
}
