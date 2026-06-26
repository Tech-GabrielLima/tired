//! Diagnostics: structured compiler errors with a `rustc`-style renderer.
//!
//! A [`Diagnostic`] carries a primary [`Span`], a message, and optional `help`/`note`
//! lines. The renderer prints the offending source line with a caret underline. The
//! [`did_you_mean`] helper powers "unknown field `reop` — did you mean `repo`?".

use crate::span::{LineMap, Span};
use std::fmt::Write as _;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Span,
    /// Secondary spans with their own labels (e.g. "first defined here").
    pub labels: Vec<(Span, String)>,
    pub help: Option<String>,
    pub note: Option<String>,
}

impl Diagnostic {
    pub fn error(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span,
            labels: Vec::new(),
            help: None,
            note: None,
        }
    }

    pub fn warning(span: Span, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            message: message.into(),
            span,
            labels: Vec::new(),
            help: None,
            note: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn with_label(mut self, span: Span, label: impl Into<String>) -> Self {
        self.labels.push((span, label.into()));
        self
    }
}

/// A collection of diagnostics produced by one compiler stage (or the whole run).
#[derive(Default, Debug)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Diagnostics::default()
    }

    pub fn push(&mut self, d: Diagnostic) {
        self.items.push(d);
    }

    pub fn extend(&mut self, other: Diagnostics) {
        self.items.extend(other.items);
    }

    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Render every diagnostic against `src`, returning a single string. `path` is
    /// shown in the `--> path:line:col` location line.
    pub fn render(&self, src: &str, path: &str) -> String {
        let map = LineMap::new(src);
        let mut out = String::new();
        for d in &self.items {
            render_one(&mut out, d, &map, path);
            out.push('\n');
        }
        out
    }
}

fn render_one(out: &mut String, d: &Diagnostic, map: &LineMap, path: &str) {
    let (line, col) = map.locate(d.span.start);
    let _ = writeln!(out, "{}: {}", d.severity.label(), d.message);
    let _ = writeln!(out, "  --> {path}:{line}:{col}");

    let gutter_w = line.to_string().len().max(1);
    let pad = " ".repeat(gutter_w);
    let _ = writeln!(out, "{pad} |");

    let text = map.line_text(line);
    let _ = writeln!(out, "{line:>gutter_w$} | {text}", gutter_w = gutter_w);

    // Caret underline. Column is 1-based; tabs are treated as one column.
    let underline_len = d.span.len().max(1);
    let caret = "^".repeat(underline_len);
    let lead = " ".repeat(col.saturating_sub(1));
    let _ = writeln!(out, "{pad} | {lead}{caret}");

    for (lspan, label) in &d.labels {
        let (lline, lcol) = map.locate(lspan.start);
        let _ = writeln!(out, "{pad} = at {path}:{lline}:{lcol}: {label}");
    }
    if let Some(help) = &d.help {
        let _ = writeln!(out, "{pad} = help: {help}");
    }
    if let Some(note) = &d.note {
        let _ = writeln!(out, "{pad} = note: {note}");
    }
}

/// Returns the closest candidate to `name` (by Levenshtein distance) if one is "near
/// enough" to be worth suggesting. Used for "did you mean" hints on typos.
pub fn did_you_mean<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for cand in candidates {
        let d = levenshtein(name, cand);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    // Only suggest when the edit distance is small relative to the word length:
    // a third of the longer word, capped at 3 edits. Avoids absurd suggestions.
    best.and_then(|(d, cand)| {
        let threshold = (name.len().max(cand.len()) / 3).clamp(1, 3);
        if d <= threshold {
            Some(cand.to_string())
        } else {
            None
        }
    })
}

/// Optimal string alignment (restricted Damerau-Levenshtein) distance over Unicode
/// scalar values. Unlike plain Levenshtein it counts an adjacent transposition as a
/// single edit, so the extremely common `reop`→`repo` typo costs 1, not 2.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_typo() {
        assert_eq!(
            did_you_mean("reop", ["repo", "stars", "owner"]),
            Some("repo".into())
        );
        assert_eq!(did_you_mean("repo", ["repo"]), Some("repo".into()));
    }

    #[test]
    fn declines_far_typo() {
        assert_eq!(did_you_mean("zzzzzz", ["repo", "stars"]), None);
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
    }
}
