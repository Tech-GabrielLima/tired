//! Source positions. A [`Span`] is a half-open byte range `[start, end)` into the
//! original source text. Everything downstream (diagnostics, the type checker, the
//! IR) carries spans so an error can always point back at the exact characters that
//! caused it.

/// A half-open byte range into a source file.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "span start must not exceed end");
        Span { start, end }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn merge(self, other: Span) -> Span {
        Span::new(self.start.min(other.start), self.end.max(other.end))
    }

    pub fn len(self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A value paired with the span it originated from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}

/// Translates byte offsets into 1-based line/column pairs for human-readable output.
pub struct LineMap<'a> {
    src: &'a str,
    /// Byte offset of the start of each line.
    line_starts: Vec<usize>,
}

impl<'a> LineMap<'a> {
    pub fn new(src: &'a str) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        LineMap { src, line_starts }
    }

    /// Returns the 1-based `(line, column)` of a byte offset.
    pub fn locate(&self, offset: usize) -> (usize, usize) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(next) => next - 1,
        };
        let col = offset - self.line_starts[line] + 1;
        (line + 1, col)
    }

    /// Returns the text of the 1-based line `line` (without the trailing newline).
    pub fn line_text(&self, line: usize) -> &'a str {
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .map(|&n| n - 1)
            .unwrap_or(self.src.len());
        &self.src[start..end.min(self.src.len())]
    }
}
