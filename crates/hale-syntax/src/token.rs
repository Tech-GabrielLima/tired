//! Token definitions shared by the lexer and parser.

use crate::span::Span;

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Literals
    Int(i64),
    Float(f64),
    /// A string literal, already unescaped. Interpolation segments are reparsed by
    /// the parser from the raw source slice (see [`Token::span`]).
    Str(String),
    /// A duration literal such as `5s`, `300ms`, `5min`, normalised to milliseconds.
    Duration(u64),
    Ident(String),
    /// `$NAME` — an environment-variable reference.
    EnvVar(String),

    // Keywords
    Endpoint,
    Type,
    Contract,
    Flow,
    Fetch,
    Parallel,
    Match,
    Mock,
    Test,
    Using,
    Assert,
    Let,
    Log,
    Return,
    Params,
    Server,
    Route,
    Budget,
    Idempotent,
    Where,
    Retry,
    Wait,
    Then,
    For,
    In,
    By,
    Asc,
    Desc,
    And,
    Or,
    Not,
    True,
    False,
    Null,

    // Punctuation / operators
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    DotDot,
    DotDotDot,
    Arrow,    // ->
    FatArrow, // =>
    Pipe,     // |
    Question, // ?
    Assign,   // =
    EqEq,     // ==
    NotEq,    // !=
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,

    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl TokenKind {
    /// A short human-readable description used in parser error messages.
    pub fn describe(&self) -> String {
        use TokenKind::*;
        match self {
            Int(_) => "an integer".into(),
            Float(_) => "a float".into(),
            Str(_) => "a string".into(),
            Duration(_) => "a duration".into(),
            Ident(s) => format!("`{s}`"),
            EnvVar(s) => format!("`${s}`"),
            Eof => "end of input".into(),
            other => format!("`{}`", other.symbol()),
        }
    }

    /// The canonical surface text for fixed tokens (keywords/punctuation).
    pub fn symbol(&self) -> &'static str {
        use TokenKind::*;
        match self {
            Endpoint => "endpoint",
            Type => "type",
            Contract => "contract",
            Flow => "flow",
            Fetch => "fetch",
            Parallel => "parallel",
            Match => "match",
            Mock => "mock",
            Test => "test",
            Using => "using",
            Assert => "assert",
            Let => "let",
            Log => "log",
            Return => "return",
            Params => "params",
            Server => "server",
            Route => "route",
            Budget => "budget",
            Idempotent => "idempotent",
            Where => "where",
            Retry => "retry",
            Wait => "wait",
            Then => "then",
            For => "for",
            In => "in",
            By => "by",
            Asc => "asc",
            Desc => "desc",
            And => "and",
            Or => "or",
            Not => "not",
            True => "true",
            False => "false",
            Null => "null",
            LBrace => "{",
            RBrace => "}",
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            Comma => ",",
            Colon => ":",
            Dot => ".",
            DotDot => "..",
            DotDotDot => "...",
            Arrow => "->",
            FatArrow => "=>",
            Pipe => "|",
            Question => "?",
            Assign => "=",
            EqEq => "==",
            NotEq => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            _ => "<value>",
        }
    }
}

impl TokenKind {
    /// If this token is a keyword, returns its surface text. Lets keywords double as
    /// ordinary names in unambiguous positions (e.g. `retry:` as an endpoint setting
    /// key, or a record field literally called `type`).
    pub fn keyword_text(&self) -> Option<&'static str> {
        use TokenKind::*;
        match self {
            Endpoint | Type | Contract | Flow | Fetch | Parallel | Match | Mock | Test | Using
            | Assert | Let | Log | Return | Params | Server | Route | Budget | Idempotent
            | Where | Retry | Wait | Then | For | In | By | Asc | Desc | And | Or | Not | True
            | False | Null => Some(self.symbol()),
            _ => None,
        }
    }
}

/// Maps an identifier to its keyword token, if it is one.
pub(crate) fn keyword(ident: &str) -> Option<TokenKind> {
    use TokenKind::*;
    Some(match ident {
        "endpoint" => Endpoint,
        "type" => Type,
        "contract" => Contract,
        "flow" => Flow,
        "fetch" => Fetch,
        "parallel" => Parallel,
        "match" => Match,
        "mock" => Mock,
        "test" => Test,
        "using" => Using,
        "assert" => Assert,
        "let" => Let,
        "log" => Log,
        "return" => Return,
        "params" => Params,
        "server" => Server,
        "route" => Route,
        "budget" => Budget,
        "idempotent" => Idempotent,
        "where" => Where,
        "retry" => Retry,
        "wait" => Wait,
        "then" => Then,
        "for" => For,
        "in" => In,
        "by" => By,
        "asc" => Asc,
        "desc" => Desc,
        "and" => And,
        "or" => Or,
        "not" => Not,
        "true" => True,
        "false" => False,
        "null" => Null,
        _ => return None,
    })
}
