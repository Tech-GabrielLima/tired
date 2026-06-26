//! Hand-written lexer. Produces a flat `Vec<Token>` (always terminated by `Eof`).
//!
//! Notable bits of the hale grammar handled here:
//! * duration literals — `5s`, `300ms`, `5min`, `2h` — normalised to milliseconds;
//! * `$NAME` environment-variable references;
//! * `..` ranges disambiguated from float literals (`1..100` is not `1.` then `.100`);
//! * string literals keep their *raw* inner text so the parser can later split out
//!   `{interpolation}` segments without the lexer needing expression context.

use crate::diag::{Diagnostic, Diagnostics};
use crate::span::Span;
use crate::token::{keyword, Token, TokenKind};

pub fn lex(src: &str) -> (Vec<Token>, Diagnostics) {
    let mut lx = Lexer {
        bytes: src.as_bytes(),
        src,
        pos: 0,
        tokens: Vec::new(),
        diags: Diagnostics::new(),
    };
    lx.run();
    (lx.tokens, lx.diags)
}

struct Lexer<'a> {
    bytes: &'a [u8],
    src: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    diags: Diagnostics,
}

impl Lexer<'_> {
    fn run(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                b' ' | b'\t' | b'\r' | b'\n' => {
                    self.pos += 1;
                }
                b'/' if self.peek_at(1) == Some(b'/') => self.skip_line_comment(),
                b'"' => self.lex_string(),
                b'$' => self.lex_env_var(),
                c if c.is_ascii_digit() => self.lex_number(),
                c if is_ident_start(c) => self.lex_ident(),
                _ => self.lex_symbol(),
            }
        }
        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(self.pos, self.pos),
        });
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn skip_line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if c == b'\n' {
                break;
            }
            self.pos += 1;
        }
    }

    fn lex_ident(&mut self) {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        let kind = keyword(text).unwrap_or_else(|| TokenKind::Ident(text.to_string()));
        self.push(kind, start);
    }

    fn lex_env_var(&mut self) {
        let start = self.pos;
        self.pos += 1; // consume '$'
        let name_start = self.pos;
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == name_start {
            self.diags.push(
                Diagnostic::error(Span::new(start, self.pos), "expected a name after `$`")
                    .with_help("environment variables are written `$NAME`, e.g. `$GITHUB_TOKEN`"),
            );
        }
        let name = self.src[name_start..self.pos].to_string();
        self.push(TokenKind::EnvVar(name), start);
    }

    fn lex_number(&mut self) {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        // A '.' is a decimal point only when followed by a digit; otherwise it is the
        // start of a `..` range or a field access and the number ends here.
        let is_float =
            self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit());
        if is_float {
            self.pos += 1; // '.'
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
            let text = &self.src[start..self.pos];
            match text.parse::<f64>() {
                Ok(v) => self.push(TokenKind::Float(v), start),
                Err(_) => {
                    self.diags.push(Diagnostic::error(
                        Span::new(start, self.pos),
                        "invalid float literal",
                    ));
                    self.push(TokenKind::Float(0.0), start);
                }
            }
            return;
        }

        // Possible duration suffix: read a trailing alpha run and see if it's a unit.
        let digits = &self.src[start..self.pos];
        let suffix_start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        let suffix = &self.src[suffix_start..self.pos];
        if suffix.is_empty() {
            self.emit_int(digits, start);
            return;
        }
        match unit_to_ms(suffix) {
            Some(scale) => {
                let n: u64 = digits.parse().unwrap_or(0);
                self.push(TokenKind::Duration(n.saturating_mul(scale)), start);
            }
            None => {
                // Not a time unit: rewind the suffix and emit a bare integer; the
                // letters will be lexed as their own identifier token next.
                self.pos = suffix_start;
                self.emit_int(digits, start);
            }
        }
    }

    fn emit_int(&mut self, digits: &str, start: usize) {
        match digits.parse::<i64>() {
            Ok(v) => self.push(TokenKind::Int(v), start),
            Err(_) => {
                self.diags.push(Diagnostic::error(
                    Span::new(start, self.pos),
                    "integer literal is too large",
                ));
                self.push(TokenKind::Int(0), start);
            }
        }
    }

    fn lex_string(&mut self) {
        let start = self.pos;
        self.pos += 1; // opening quote
        let inner_start = self.pos;
        let mut escaped = false;
        loop {
            match self.peek() {
                None => {
                    self.diags.push(Diagnostic::error(
                        Span::new(start, self.pos),
                        "unterminated string literal",
                    ));
                    break;
                }
                Some(b'\\') if !escaped => {
                    escaped = true;
                    self.pos += 1;
                }
                Some(b'"') if !escaped => {
                    let raw = self.src[inner_start..self.pos].to_string();
                    self.pos += 1; // closing quote
                    self.push(TokenKind::Str(raw), start);
                    return;
                }
                Some(_) => {
                    escaped = false;
                    self.pos += 1;
                }
            }
        }
        // Unterminated: emit what we have so parsing can continue.
        let raw = self.src[inner_start..self.pos].to_string();
        self.push(TokenKind::Str(raw), start);
    }

    fn lex_symbol(&mut self) {
        let start = self.pos;
        let c = self.peek().unwrap();
        let two = self.peek_at(1);
        macro_rules! single {
            ($k:expr) => {{
                self.pos += 1;
                self.push($k, start);
            }};
        }
        macro_rules! double {
            ($k:expr) => {{
                self.pos += 2;
                self.push($k, start);
            }};
        }
        use TokenKind::*;
        match (c, two) {
            (b'-', Some(b'>')) => double!(Arrow),
            (b'=', Some(b'>')) => double!(FatArrow),
            (b'=', Some(b'=')) => double!(EqEq),
            (b'!', Some(b'=')) => double!(NotEq),
            (b'<', Some(b'=')) => double!(Le),
            (b'>', Some(b'=')) => double!(Ge),
            (b'.', Some(b'.')) => {
                if self.peek_at(2) == Some(b'.') {
                    self.pos += 3;
                    self.push(DotDotDot, start);
                } else {
                    double!(DotDot)
                }
            }
            (b'{', _) => single!(LBrace),
            (b'}', _) => single!(RBrace),
            (b'(', _) => single!(LParen),
            (b')', _) => single!(RParen),
            (b'[', _) => single!(LBracket),
            (b']', _) => single!(RBracket),
            (b',', _) => single!(Comma),
            (b':', _) => single!(Colon),
            (b'.', _) => single!(Dot),
            (b'|', _) => single!(Pipe),
            (b'?', _) => single!(Question),
            (b'=', _) => single!(Assign),
            (b'<', _) => single!(Lt),
            (b'>', _) => single!(Gt),
            (b'+', _) => single!(Plus),
            (b'-', _) => single!(Minus),
            (b'*', _) => single!(Star),
            (b'/', _) => single!(Slash),
            _ => {
                self.pos += 1;
                self.diags.push(Diagnostic::error(
                    Span::new(start, self.pos),
                    format!("unexpected character `{}`", c as char),
                ));
            }
        }
    }
}

fn unit_to_ms(unit: &str) -> Option<u64> {
    Some(match unit {
        "ms" => 1,
        "s" | "sec" => 1_000,
        "m" | "min" => 60_000,
        "h" | "hr" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    })
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::TokenKind::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        let (toks, diags) = lex(src);
        assert!(
            !diags.has_errors(),
            "unexpected lex errors: {:?}",
            diags.items()
        );
        toks.into_iter()
            .map(|t| t.kind)
            .filter(|k| *k != Eof)
            .collect()
    }

    #[test]
    fn durations_and_ranges() {
        assert_eq!(
            kinds("5s 300ms 5min 2h"),
            vec![
                Duration(5000),
                Duration(300),
                Duration(300_000),
                Duration(7_200_000)
            ]
        );
        assert_eq!(kinds("1..100"), vec![Int(1), DotDot, Int(100)]);
        assert_eq!(kinds("0.5"), vec![Float(0.5)]);
    }

    #[test]
    fn pipeline_and_arrows() {
        assert_eq!(
            kinds("fetch GitHub | filter -> x"),
            vec![
                Fetch,
                Ident("GitHub".into()),
                Pipe,
                Ident("filter".into()),
                Arrow,
                Ident("x".into())
            ]
        );
    }

    #[test]
    fn env_var_and_string() {
        let ks = kinds(r#"auth: Bearer($TOKEN) "hi {x}""#);
        assert!(ks.contains(&EnvVar("TOKEN".into())));
        assert!(ks.iter().any(|k| matches!(k, Str(s) if s == "hi {x}")));
    }

    #[test]
    fn field_access_not_float() {
        assert_eq!(
            kinds("repo.stars"),
            vec![Ident("repo".into()), Dot, Ident("stars".into())]
        );
    }
}
