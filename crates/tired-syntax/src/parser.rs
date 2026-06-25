//! Recursive-descent parser with a Pratt expression sub-parser. On error it records
//! a [`Diagnostic`] and recovers to the next top-level item so a single typo does not
//! cascade into a wall of spurious errors.

use crate::ast::*;
use crate::diag::{Diagnostic, Diagnostics};
use crate::lexer::lex;
use crate::span::{Span, Spanned};
use crate::token::{Token, TokenKind};

struct ParseError;
type PResult<T> = Result<T, ParseError>;

pub fn parse(src: &str) -> (Program, Diagnostics) {
    let (tokens, mut diags) = lex(src);
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        prev: Span::default(),
        diags: Diagnostics::new(),
        no_record: false,
    };
    let program = p.parse_program();
    diags.extend(p.diags);
    (program, diags)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    prev: Span,
    diags: Diagnostics,
    /// When set, an `ident {` is *not* treated as a record literal. Used while parsing
    /// a `match` scrutinee so the following `{ arms }` is not eaten as a record (the
    /// classic struct-literal-in-condition ambiguity). Reset inside `(...)`/`[...]`.
    no_record: bool,
}

impl Parser {
    // ---------- cursor primitives ----------

    fn cur(&self) -> &TokenKind {
        &self.toks[self.pos].kind
    }
    fn cur_span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn nth(&self, n: usize) -> &TokenKind {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].kind
    }
    fn at(&self, k: &TokenKind) -> bool {
        std::mem::discriminant(self.cur()) == std::mem::discriminant(k)
    }
    fn at_eof(&self) -> bool {
        matches!(self.cur(), TokenKind::Eof)
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        self.prev = t.span;
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, k: &TokenKind) -> bool {
        if self.at(k) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn ident_text(&self) -> Option<&str> {
        if let TokenKind::Ident(s) = self.cur() {
            Some(s)
        } else {
            None
        }
    }

    fn err<T>(&mut self, span: Span, msg: impl Into<String>) -> PResult<T> {
        self.diags.push(Diagnostic::error(span, msg));
        Err(ParseError)
    }

    fn expect(&mut self, k: TokenKind) -> PResult<Token> {
        if self.at(&k) {
            Ok(self.bump())
        } else {
            let span = self.cur_span();
            let found = self.cur().describe();
            self.err(span, format!("expected {}, found {}", k.describe(), found))
        }
    }

    fn take_ident(&mut self) -> PResult<Name> {
        if let TokenKind::Ident(_) = self.cur() {
            let t = self.bump();
            if let TokenKind::Ident(s) = t.kind {
                return Ok(Spanned::new(s, t.span));
            }
        }
        let span = self.cur_span();
        let found = self.cur().describe();
        self.err(span, format!("expected an identifier, found {found}"))
    }

    /// Like [`take_ident`], but also accepts a keyword token as a plain name. Used in
    /// positions where a name is unambiguously expected (setting/record/field keys),
    /// so e.g. `retry` can be both a keyword and an endpoint setting key.
    fn take_name_lenient(&mut self) -> PResult<Name> {
        if let TokenKind::Ident(_) = self.cur() {
            return self.take_ident();
        }
        if let Some(text) = self.cur().keyword_text() {
            let t = self.bump();
            return Ok(Spanned::new(text.to_string(), t.span));
        }
        let span = self.cur_span();
        let found = self.cur().describe();
        self.err(span, format!("expected a name, found {found}"))
    }

    // ---------- program / items ----------

    fn parse_program(&mut self) -> Program {
        let mut items = Vec::new();
        while !self.at_eof() {
            let before = self.pos;
            match self.parse_item() {
                Ok(item) => items.push(item),
                Err(_) => self.recover_to_item(),
            }
            // Guarantee forward progress even if recovery left us in place.
            if self.pos == before && !self.at_eof() {
                self.bump();
            }
        }
        Program { items }
    }

    /// Skip tokens until something that plausibly begins a new top-level item.
    fn recover_to_item(&mut self) {
        while !self.at_eof() {
            if matches!(
                self.cur(),
                TokenKind::Endpoint
                    | TokenKind::Type
                    | TokenKind::Contract
                    | TokenKind::Flow
                    | TokenKind::Mock
                    | TokenKind::Test
                    | TokenKind::Fetch
                    | TokenKind::Let
                    | TokenKind::Log
                    | TokenKind::Parallel
            ) {
                return;
            }
            self.bump();
        }
    }

    fn parse_item(&mut self) -> PResult<Item> {
        match self.cur() {
            TokenKind::Endpoint => Ok(Item::Endpoint(self.parse_endpoint()?)),
            TokenKind::Type | TokenKind::Contract => Ok(Item::Type(self.parse_type_decl()?)),
            TokenKind::Flow => Ok(Item::Flow(self.parse_flow()?)),
            TokenKind::Mock => Ok(Item::Mock(self.parse_mock()?)),
            TokenKind::Test => Ok(Item::Test(self.parse_test()?)),
            _ => Ok(Item::Stmt(self.parse_stmt()?)),
        }
    }

    // ---------- endpoint ----------

    fn parse_endpoint(&mut self) -> PResult<EndpointDecl> {
        let start = self.cur_span();
        self.bump(); // endpoint
        let name = self.take_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut settings = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            let s_start = self.cur_span();
            let key = self.take_name_lenient()?;
            self.expect(TokenKind::Colon)?;
            let values = self.parse_setting_values()?;
            settings.push(Setting {
                key,
                values,
                span: s_start.merge(self.prev),
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok(EndpointDecl {
            name,
            settings,
            span: start.merge(self.prev),
        })
    }

    /// Parse one or more space-separated atoms until the next `key:` or `}`.
    fn parse_setting_values(&mut self) -> PResult<Vec<Expr>> {
        let mut vals = Vec::new();
        loop {
            vals.push(self.parse_expr()?);
            if self.at(&TokenKind::RBrace) || self.at_eof() {
                break;
            }
            // A following `name :` starts the next setting (the name may be a keyword
            // doubling as a setting key, e.g. `retry:`).
            let next_is_key =
                matches!(self.cur(), TokenKind::Ident(_)) || self.cur().keyword_text().is_some();
            if next_is_key && matches!(self.nth(1), TokenKind::Colon) {
                break;
            }
        }
        Ok(vals)
    }

    // ---------- type / contract ----------

    fn parse_type_decl(&mut self) -> PResult<TypeDecl> {
        let start = self.cur_span();
        let is_contract = matches!(self.cur(), TokenKind::Contract);
        self.bump();
        // Optionally qualified: `contract GitHub.Repo` defines type `Repo`.
        let mut name = self.take_ident()?;
        if self.eat(&TokenKind::Dot) {
            name = self.take_ident()?;
        }
        self.expect(TokenKind::LBrace)?;
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            let f_start = self.cur_span();
            let fname = self.take_name_lenient()?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            let constraint = if self.eat(&TokenKind::Where) {
                self.expect(TokenKind::LParen)?;
                let c = self.parse_constraint()?;
                self.expect(TokenKind::RParen)?;
                Some(c)
            } else {
                None
            };
            self.eat(&TokenKind::Comma);
            fields.push(FieldDecl {
                name: fname,
                ty,
                constraint,
                span: f_start.merge(self.prev),
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok(TypeDecl {
            is_contract,
            name,
            fields,
            span: start.merge(self.prev),
        })
    }

    fn parse_constraint(&mut self) -> PResult<Constraint> {
        let subject = if self.ident_text() == Some("length") {
            self.bump();
            ConstraintSubject::Length
        } else {
            ConstraintSubject::Value
        };
        if self.eat(&TokenKind::In) {
            let lo = self.parse_expr()?;
            self.expect(TokenKind::DotDot)?;
            let hi = self.parse_expr()?;
            return Ok(Constraint::InRange { subject, lo, hi });
        }
        let op = self.parse_comparison_op()?;
        let rhs = self.parse_expr()?;
        Ok(Constraint::Cmp { subject, op, rhs })
    }

    fn parse_comparison_op(&mut self) -> PResult<BinOp> {
        let op = match self.cur() {
            TokenKind::EqEq => BinOp::Eq,
            TokenKind::NotEq => BinOp::Ne,
            TokenKind::Lt => BinOp::Lt,
            TokenKind::Le => BinOp::Le,
            TokenKind::Gt => BinOp::Gt,
            TokenKind::Ge => BinOp::Ge,
            _ => {
                let span = self.cur_span();
                return self.err(
                    span,
                    "expected a comparison operator (`>`, `<=`, `==`, ...)",
                );
            }
        };
        self.bump();
        Ok(op)
    }

    // ---------- flow ----------

    fn parse_flow(&mut self) -> PResult<FlowDecl> {
        let start = self.cur_span();
        self.bump(); // flow
        let name = self.take_ident()?;
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        while !self.at(&TokenKind::RParen) && !self.at_eof() {
            let pname = self.take_ident()?;
            self.expect(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            params.push(Param { name: pname, ty });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RParen)?;
        let ret = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(FlowDecl {
            name,
            params,
            ret,
            body,
            span: start.merge(self.prev),
        })
    }

    // ---------- mock / test ----------

    fn parse_mock(&mut self) -> PResult<MockDecl> {
        let start = self.cur_span();
        self.bump(); // mock
        let name = self.take_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut routes = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            let r_start = self.cur_span();
            let method = self.take_ident()?;
            let path = self.parse_path()?;
            self.expect(TokenKind::Arrow)?;
            let response = self.parse_expr()?;
            routes.push(MockRoute {
                method,
                path,
                response,
                span: r_start.merge(self.prev),
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok(MockDecl {
            name,
            routes,
            span: start.merge(self.prev),
        })
    }

    fn parse_test(&mut self) -> PResult<TestDecl> {
        let start = self.cur_span();
        self.bump(); // test
        let description = match self.cur() {
            TokenKind::Str(_) => {
                if let TokenKind::Str(s) = self.bump().kind {
                    s
                } else {
                    unreachable!()
                }
            }
            _ => {
                let span = self.cur_span();
                return self.err(span, "expected a test description string after `test`");
            }
        };
        let body = self.parse_block()?;
        Ok(TestDecl {
            description,
            body,
            span: start.merge(self.prev),
        })
    }

    // ---------- statements ----------

    fn parse_block(&mut self) -> PResult<Block> {
        let start = self.cur_span();
        self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            stmts.push(self.parse_stmt()?);
        }
        self.expect(TokenKind::RBrace)?;
        Ok(Block {
            stmts,
            span: start.merge(self.prev),
        })
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        match self.cur() {
            TokenKind::Fetch => self.parse_fetch(),
            TokenKind::Let => {
                let start = self.cur_span();
                self.bump();
                let name = self.take_ident()?;
                self.expect(TokenKind::Assign)?;
                let value = self.parse_expr()?;
                Ok(Stmt::Let {
                    name,
                    value,
                    span: start.merge(self.prev),
                })
            }
            TokenKind::Log => {
                let start = self.cur_span();
                self.bump();
                let value = self.parse_expr()?;
                Ok(Stmt::Log {
                    value,
                    span: start.merge(self.prev),
                })
            }
            TokenKind::Parallel => {
                let start = self.cur_span();
                self.bump();
                let block = self.parse_block()?;
                Ok(Stmt::Parallel {
                    block,
                    span: start.merge(self.prev),
                })
            }
            TokenKind::Return => {
                let start = self.cur_span();
                self.bump();
                let value = if self.at(&TokenKind::RBrace) || self.at_eof() {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                Ok(Stmt::Return {
                    value,
                    span: start.merge(self.prev),
                })
            }
            TokenKind::Assert => {
                let start = self.cur_span();
                self.bump();
                let value = self.parse_expr()?;
                Ok(Stmt::Assert {
                    value,
                    span: start.merge(self.prev),
                })
            }
            TokenKind::Using => {
                let start = self.cur_span();
                self.bump();
                self.expect(TokenKind::Mock)?;
                let name = self.take_ident()?;
                Ok(Stmt::UsingMock {
                    name,
                    span: start.merge(self.prev),
                })
            }
            _ => {
                let start = self.cur_span();
                let expr = self.parse_expr()?;
                let bind = if self.eat(&TokenKind::Arrow) {
                    Some(self.parse_binding()?)
                } else {
                    None
                };
                Ok(Stmt::Expr {
                    expr,
                    bind,
                    span: start.merge(self.prev),
                })
            }
        }
    }

    fn parse_fetch(&mut self) -> PResult<Stmt> {
        let start = self.cur_span();
        self.bump(); // fetch
        let endpoint = self.take_ident()?;
        let path = self.parse_path()?;
        let mut params = Vec::new();
        if self.eat(&TokenKind::Params) {
            self.expect(TokenKind::LBrace)?;
            while !self.at(&TokenKind::RBrace) && !self.at_eof() {
                let k = self.take_name_lenient()?;
                self.expect(TokenKind::Colon)?;
                let v = self.parse_expr()?;
                params.push((k, v));
                self.eat(&TokenKind::Comma);
            }
            self.expect(TokenKind::RBrace)?;
        }
        let mut pipeline = Vec::new();
        while self.eat(&TokenKind::Pipe) {
            pipeline.push(self.parse_pipeline_op()?);
        }
        let bind = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_binding()?)
        } else {
            None
        };
        Ok(Stmt::Fetch(FetchStmt {
            endpoint,
            path,
            params,
            pipeline,
            bind,
            span: start.merge(self.prev),
        }))
    }

    fn parse_binding(&mut self) -> PResult<Binding> {
        let name = self.take_ident()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(Binding { name, ty })
    }

    fn parse_path(&mut self) -> PResult<PathPattern> {
        let start = self.cur_span();
        if !self.at(&TokenKind::Slash) {
            let span = self.cur_span();
            return self.err(span, "expected a route path starting with `/`");
        }
        let mut segments = Vec::new();
        while self.at(&TokenKind::Slash) {
            self.bump(); // '/'
            match self.cur() {
                TokenKind::LBrace => {
                    self.bump();
                    let expr = self.parse_expr()?;
                    self.expect(TokenKind::RBrace)?;
                    segments.push(PathSeg::Param(expr));
                }
                TokenKind::Ident(s) => {
                    let s = s.clone();
                    self.bump();
                    segments.push(PathSeg::Literal(s));
                }
                TokenKind::Int(n) => {
                    let n = *n;
                    self.bump();
                    segments.push(PathSeg::Literal(n.to_string()));
                }
                _ => break, // tolerate a trailing slash
            }
        }
        Ok(PathPattern {
            segments,
            span: start.merge(self.prev),
        })
    }

    fn parse_pipeline_op(&mut self) -> PResult<PipelineOp> {
        let start = self.cur_span();
        let name = self.take_ident()?;
        self.expect(TokenKind::LParen)?;
        let op = match name.node.as_str() {
            "filter" => {
                let lambda = self.parse_expr()?;
                PipelineOp::Filter {
                    lambda,
                    span: start.merge(self.cur_span()),
                }
            }
            "map" => {
                let lambda = self.parse_expr()?;
                PipelineOp::Map {
                    lambda,
                    span: start.merge(self.cur_span()),
                }
            }
            "sort" => {
                self.expect(TokenKind::By)?;
                self.expect(TokenKind::Colon)?;
                let by = self.parse_expr()?;
                let desc = if self.eat(&TokenKind::Desc) {
                    true
                } else {
                    self.eat(&TokenKind::Asc);
                    false
                };
                PipelineOp::Sort {
                    by,
                    desc,
                    span: start.merge(self.cur_span()),
                }
            }
            "limit" => {
                let count = self.parse_expr()?;
                PipelineOp::Limit {
                    count,
                    span: start.merge(self.cur_span()),
                }
            }
            other => {
                let span = name.span;
                return self.err(
                    span,
                    format!(
                        "unknown pipeline operator `{other}` (expected filter, map, sort or limit)"
                    ),
                );
            }
        };
        self.expect(TokenKind::RParen)?;
        Ok(op)
    }

    // ---------- types ----------

    fn parse_type(&mut self) -> PResult<TypeExpr> {
        let first = self.parse_type_atom()?;
        if !self.at(&TokenKind::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat(&TokenKind::Pipe) {
            alts.push(self.parse_type_atom()?);
        }
        Ok(TypeExpr::Union(alts))
    }

    fn parse_type_atom(&mut self) -> PResult<TypeExpr> {
        let name = self.take_ident()?;
        let mut base = if self.at(&TokenKind::Lt) {
            self.bump();
            let mut args = Vec::new();
            while !self.at(&TokenKind::Gt) && !self.at_eof() {
                args.push(self.parse_type()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::Gt)?;
            TypeExpr::Generic(name.node, args)
        } else {
            TypeExpr::Named(name.node)
        };
        loop {
            if self.at(&TokenKind::LBracket) && matches!(self.nth(1), TokenKind::RBracket) {
                self.bump();
                self.bump();
                base = TypeExpr::Array(Box::new(base));
            } else if self.eat(&TokenKind::Question) {
                base = TypeExpr::Optional(Box::new(base));
            } else {
                break;
            }
        }
        Ok(base)
    }

    // ---------- expressions (Pratt) ----------

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_and()?;
        while self.at(&TokenKind::Or) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = bin(BinOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_cmp()?;
        while self.at(&TokenKind::And) {
            self.bump();
            let rhs = self.parse_cmp()?;
            lhs = bin(BinOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.cur() {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::NotEq => BinOp::Ne,
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Le => BinOp::Le,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::Ge => BinOp::Ge,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_add()?;
            lhs = bin(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.cur() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul()?;
            lhs = bin(op, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        while self.at(&TokenKind::Star) {
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = bin(BinOp::Mul, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        if self.at(&TokenKind::Not) {
            self.bump();
            let rhs = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnOp::Not,
                span: start.merge(rhs.span()),
                rhs: Box::new(rhs),
            });
        }
        if self.at(&TokenKind::Minus) {
            self.bump();
            let rhs = self.parse_unary()?;
            return Ok(Expr::Unary {
                op: UnOp::Neg,
                span: start.merge(rhs.span()),
                rhs: Box::new(rhs),
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            if self.at(&TokenKind::Dot) {
                self.bump();
                let field = self.take_ident()?;
                let span = e.span().merge(field.span);
                e = Expr::Field {
                    base: Box::new(e),
                    field,
                    span,
                };
            } else if self.at(&TokenKind::LParen) {
                self.bump();
                let saved = self.no_record;
                self.no_record = false;
                let mut args = Vec::new();
                while !self.at(&TokenKind::RParen) && !self.at_eof() {
                    args.push(self.parse_expr()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.no_record = saved;
                self.expect(TokenKind::RParen)?;
                let span = e.span().merge(self.prev);
                e = Expr::Call {
                    callee: Box::new(e),
                    args,
                    span,
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let span = self.cur_span();
        match self.cur().clone() {
            TokenKind::Int(n) => {
                self.bump();
                Ok(Expr::Int(n, span))
            }
            TokenKind::Float(f) => {
                self.bump();
                Ok(Expr::Float(f, span))
            }
            TokenKind::Duration(ms) => {
                self.bump();
                Ok(Expr::Duration(ms, span))
            }
            TokenKind::True => {
                self.bump();
                Ok(Expr::Bool(true, span))
            }
            TokenKind::False => {
                self.bump();
                Ok(Expr::Bool(false, span))
            }
            TokenKind::Null => {
                self.bump();
                Ok(Expr::Null(span))
            }
            TokenKind::Str(raw) => {
                self.bump();
                let parts = self.parse_str_parts(&raw, span);
                Ok(Expr::Str { parts, span })
            }
            TokenKind::EnvVar(name) => {
                self.bump();
                Ok(Expr::EnvVar(Spanned::new(name, span)))
            }
            TokenKind::Dot => {
                self.bump();
                let field = self.take_ident()?;
                Ok(Expr::ImplicitField(field))
            }
            TokenKind::Match => Ok(Expr::Match(Box::new(self.parse_match()?))),
            TokenKind::LParen => {
                self.bump();
                let saved = self.no_record;
                self.no_record = false;
                let e = self.parse_expr()?;
                self.no_record = saved;
                self.expect(TokenKind::RParen)?;
                Ok(e)
            }
            TokenKind::LBrace => self.parse_record(None),
            TokenKind::LBracket => self.parse_array(),
            TokenKind::Ident(_) => {
                if matches!(self.nth(1), TokenKind::FatArrow) {
                    let param = self.take_ident()?;
                    self.bump(); // =>
                    let body = self.parse_expr()?;
                    let span = param.span.merge(body.span());
                    Ok(Expr::Lambda {
                        param,
                        body: Box::new(body),
                        span,
                    })
                } else if matches!(self.nth(1), TokenKind::LBrace) && !self.no_record {
                    let name = self.take_ident()?;
                    self.parse_record(Some(name))
                } else {
                    let name = self.take_ident()?;
                    Ok(Expr::Ident(name))
                }
            }
            other => self.err(
                span,
                format!("expected an expression, found {}", other.describe()),
            ),
        }
    }

    fn parse_record(&mut self, name: Option<Name>) -> PResult<Expr> {
        let start = name.as_ref().map(|n| n.span).unwrap_or(self.cur_span());
        self.expect(TokenKind::LBrace)?;
        let saved = self.no_record;
        self.no_record = false;
        let mut fields = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            let k = self.take_name_lenient()?;
            self.expect(TokenKind::Colon)?;
            let v = self.parse_expr()?;
            fields.push((k, v));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.no_record = saved;
        self.expect(TokenKind::RBrace)?;
        Ok(Expr::Record {
            name,
            fields,
            span: start.merge(self.prev),
        })
    }

    fn parse_array(&mut self) -> PResult<Expr> {
        let start = self.cur_span();
        self.expect(TokenKind::LBracket)?;
        let saved = self.no_record;
        self.no_record = false;
        let mut elems = Vec::new();
        while !self.at(&TokenKind::RBracket) && !self.at_eof() {
            if self.at(&TokenKind::DotDotDot) {
                let s = self.cur_span();
                self.bump();
                let e = self.parse_expr()?;
                let span = s.merge(e.span());
                elems.push(Expr::Spread {
                    expr: Box::new(e),
                    span,
                });
            } else {
                elems.push(self.parse_expr()?);
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.no_record = saved;
        self.expect(TokenKind::RBracket)?;
        Ok(Expr::Array {
            elems,
            span: start.merge(self.prev),
        })
    }

    fn parse_match(&mut self) -> PResult<MatchExpr> {
        let start = self.cur_span();
        self.bump(); // match
        let saved = self.no_record;
        self.no_record = true;
        let scrutinee = self.parse_expr()?;
        self.no_record = saved;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.at_eof() {
            let a_start = self.cur_span();
            let pattern = self.parse_pattern()?;
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_arm_body()?;
            self.eat(&TokenKind::Comma);
            arms.push(MatchArm {
                pattern,
                body,
                span: a_start.merge(self.prev),
            });
        }
        self.expect(TokenKind::RBrace)?;
        Ok(MatchExpr {
            scrutinee,
            arms,
            span: start.merge(self.prev),
        })
    }

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        if self.ident_text() == Some("_") {
            let span = self.cur_span();
            self.bump();
            return Ok(Pattern::Wildcard(span));
        }
        let name = self.take_ident()?;
        if self.at(&TokenKind::LParen) {
            self.bump();
            let mut args = Vec::new();
            while !self.at(&TokenKind::RParen) && !self.at_eof() {
                args.push(self.parse_pattern()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RParen)?;
            let span = name.span.merge(self.prev);
            return Ok(Pattern::Ctor { name, args, span });
        }
        // Capitalised bare name = nullary constructor; lowercase = binding.
        if name.node.chars().next().is_some_and(|c| c.is_uppercase()) {
            Ok(Pattern::Ctor {
                span: name.span,
                name,
                args: Vec::new(),
            })
        } else {
            Ok(Pattern::Binding(name))
        }
    }

    fn parse_arm_body(&mut self) -> PResult<ArmBody> {
        if self.at(&TokenKind::LBrace) {
            return Ok(ArmBody::Block(self.parse_block()?));
        }
        let start = self.cur_span();
        let mut effects = Vec::new();
        loop {
            if self.eat(&TokenKind::Retry) {
                return Ok(ArmBody::Retry {
                    effects,
                    span: start.merge(self.prev),
                });
            }
            if self.at(&TokenKind::Wait) {
                self.bump();
                self.expect(TokenKind::LParen)?;
                let e = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                effects.push(Effect::Wait(e));
                self.expect(TokenKind::Then)?;
                continue;
            }
            let e = self.parse_expr()?;
            if self.eat(&TokenKind::Then) {
                effects.push(Effect::Call(e));
                continue;
            }
            if !effects.is_empty() {
                let span = e.span();
                return self.err(span, "expected `retry` after `then`");
            }
            return Ok(ArmBody::Value(e));
        }
    }

    // ---------- string interpolation ----------

    /// Split a raw string body into literal and `{interpolation}` parts. `{{`/`}}`
    /// are literal braces; escape sequences in literal text are processed here.
    fn parse_str_parts(&mut self, raw: &str, span: Span) -> Vec<StrPart> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        let chars: Vec<char> = raw.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            match c {
                '{' if chars.get(i + 1) == Some(&'{') => {
                    lit.push('{');
                    i += 2;
                }
                '}' if chars.get(i + 1) == Some(&'}') => {
                    lit.push('}');
                    i += 2;
                }
                '{' => {
                    if !lit.is_empty() {
                        parts.push(StrPart::Lit(std::mem::take(&mut lit)));
                    }
                    // Collect until the matching '}' (no nested braces inside interpolation).
                    let mut depth = 1;
                    let mut inner = String::new();
                    i += 1;
                    while i < chars.len() && depth > 0 {
                        match chars[i] {
                            '{' => {
                                depth += 1;
                                inner.push('{');
                            }
                            '}' => {
                                depth -= 1;
                                if depth > 0 {
                                    inner.push('}');
                                }
                            }
                            other => inner.push(other),
                        }
                        i += 1;
                    }
                    match self.parse_subexpr(&inner) {
                        Some(expr) => parts.push(StrPart::Interp(expr)),
                        None => self.diags.push(Diagnostic::error(
                            span,
                            format!("invalid interpolation `{{{inner}}}` in string literal"),
                        )),
                    }
                }
                '\\' => {
                    i += 1;
                    if let Some(&esc) = chars.get(i) {
                        lit.push(match esc {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '"' => '"',
                            '\\' => '\\',
                            '{' => '{',
                            '}' => '}',
                            other => other,
                        });
                        i += 1;
                    }
                }
                other => {
                    lit.push(other);
                    i += 1;
                }
            }
        }
        if !lit.is_empty() || parts.is_empty() {
            parts.push(StrPart::Lit(lit));
        }
        parts
    }

    /// Parse a single expression from an isolated source fragment (string interpolation).
    fn parse_subexpr(&mut self, src: &str) -> Option<Expr> {
        let (tokens, diags) = lex(src);
        if diags.has_errors() {
            return None;
        }
        let mut sub = Parser {
            toks: tokens,
            pos: 0,
            prev: Span::default(),
            diags: Diagnostics::new(),
            no_record: false,
        };
        let expr = sub.parse_expr().ok()?;
        if sub.diags.has_errors() || !sub.at_eof() {
            return None;
        }
        Some(expr)
    }
}

fn bin(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    let span = lhs.span().merge(rhs.span());
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Program {
        let (prog, diags) = parse(src);
        assert!(
            !diags.has_errors(),
            "unexpected parse errors:\n{}",
            diags.render(src, "test")
        );
        prog
    }

    #[test]
    fn parses_endpoint() {
        let p = ok(
            r#"endpoint GitHub { base: "https://api.github.com" timeout: 5s retry: 3 backoff(exponential) }"#,
        );
        assert_eq!(p.items.len(), 1);
        match &p.items[0] {
            Item::Endpoint(e) => {
                assert_eq!(e.name.node, "GitHub");
                assert_eq!(e.settings.len(), 3);
                assert_eq!(e.settings[2].values.len(), 2); // 3, backoff(exponential)
            }
            _ => panic!("expected endpoint"),
        }
    }

    #[test]
    fn parses_fetch_with_pipeline() {
        let p = ok("fetch GitHub /users/{username}/repos | filter(repo => repo.stars > 100) | sort(by: .updated_at desc) | limit(10) -> repos: Repo[]");
        match &p.items[0] {
            Item::Stmt(Stmt::Fetch(f)) => {
                assert_eq!(f.endpoint.node, "GitHub");
                assert_eq!(f.pipeline.len(), 3);
                assert!(f.bind.is_some());
            }
            _ => panic!("expected fetch"),
        }
    }

    #[test]
    fn parses_match_with_retry() {
        let p = ok(r#"
            flow F() -> Charge {
                fetch Stripe /charges/{id} -> result: Result<Charge, ApiError>
                match result {
                    Ok(charge) => charge
                    Err(NotFound) => default_charge()
                    Err(RateLimit(ms)) => wait(ms) then retry
                    _ => other()
                }
            }
        "#);
        match &p.items[0] {
            Item::Flow(f) => {
                assert_eq!(f.body.stmts.len(), 2);
            }
            _ => panic!("expected flow"),
        }
    }

    #[test]
    fn parses_parallel_and_records() {
        ok(r#"
            fetch GitHub /users/gabriel -> user
            parallel {
                fetch GitHub /users/{user.login}/repos -> repos
                fetch GitHub /users/{user.login}/starred -> starred
            }
            log "user {user.login} has data"
        "#);
    }

    #[test]
    fn parses_string_interpolation() {
        let p = ok(r#"log "Charge {charge.id}: ${charge.amount}""#);
        match &p.items[0] {
            Item::Stmt(Stmt::Log {
                value: Expr::Str { parts, .. },
                ..
            }) => {
                // "Charge ", {charge.id}, ": $", {charge.amount}
                assert!(parts.len() >= 3);
            }
            _ => panic!("expected log of string"),
        }
    }
}
