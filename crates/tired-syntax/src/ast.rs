//! The TIRED abstract syntax tree. Nodes carry [`Span`]s so later stages can report
//! precise errors. This is the surface syntax — the compiler lowers it to an IR
//! ([`tired_compiler::ir`]) before optimization and execution.

use crate::span::{Span, Spanned};

pub type Name = Spanned<String>;

#[derive(Clone, Debug)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub enum Item {
    Endpoint(EndpointDecl),
    Type(TypeDecl),
    Flow(FlowDecl),
    Mock(MockDecl),
    Test(TestDecl),
    Server(ServerDecl),
    /// A top-level statement (the script "main").
    Stmt(Stmt),
}

/// An HTTP server: a set of routes whose handlers consume other endpoints. Each handler
/// goes through the same optimizer, so a fan-out aggregation parallelizes itself.
#[derive(Clone, Debug)]
pub struct ServerDecl {
    pub name: Name,
    pub settings: Vec<Setting>,
    pub routes: Vec<ServerRoute>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ServerRoute {
    pub method: Name,
    pub path: PathPattern,
    /// Handler body; path parameters (plus `query` and `body`) are in scope.
    pub handler: Block,
    pub span: Span,
}

// ---------- declarations ----------

#[derive(Clone, Debug)]
pub struct EndpointDecl {
    pub name: Name,
    pub settings: Vec<Setting>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Setting {
    pub key: Name,
    /// A setting value is one or more space-separated atoms, e.g. `3 backoff(exponential)`.
    pub values: Vec<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    /// `true` for `contract` (response is runtime-validated), `false` for `type`.
    pub is_contract: bool,
    pub name: Name,
    pub fields: Vec<FieldDecl>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub name: Name,
    pub ty: TypeExpr,
    pub constraint: Option<Constraint>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeExpr {
    Named(String),
    Optional(Box<TypeExpr>),
    Array(Box<TypeExpr>),
    Generic(String, Vec<TypeExpr>),
    /// A sum of alternatives, e.g. `NotFound | Unauthorized` (mostly used in the error
    /// position of `Result<T, E>`).
    Union(Vec<TypeExpr>),
}

#[derive(Clone, Debug)]
pub enum ConstraintSubject {
    /// The field value itself.
    Value,
    /// The field value's length (strings / arrays).
    Length,
}

#[derive(Clone, Debug)]
pub enum Constraint {
    Cmp {
        subject: ConstraintSubject,
        op: BinOp,
        rhs: Expr,
    },
    InRange {
        subject: ConstraintSubject,
        lo: Expr,
        hi: Expr,
    },
}

#[derive(Clone, Debug)]
pub struct FlowDecl {
    pub name: Name,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: Name,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug)]
pub struct MockDecl {
    pub name: Name,
    pub routes: Vec<MockRoute>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct MockRoute {
    pub method: Name,
    pub path: PathPattern,
    pub response: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TestDecl {
    pub description: String,
    pub body: Block,
    pub span: Span,
}

// ---------- statements ----------

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Fetch(FetchStmt),
    Let {
        name: Name,
        value: Expr,
        span: Span,
    },
    Log {
        value: Expr,
        span: Span,
    },
    Parallel {
        block: Block,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    Assert {
        value: Expr,
        span: Span,
    },
    /// `using mock NAME` — install a mock for the enclosing test.
    UsingMock {
        name: Name,
        span: Span,
    },
    /// A bare expression, optionally bound with `-> name`.
    Expr {
        expr: Expr,
        bind: Option<Binding>,
        span: Span,
    },
}

#[derive(Clone, Debug)]
pub struct FetchStmt {
    /// HTTP method, uppercased. Defaults to `GET`.
    pub method: String,
    pub endpoint: Name,
    pub path: PathPattern,
    pub params: Vec<(Name, Expr)>,
    /// Optional request body (`body <expr>`), sent as JSON.
    pub body: Option<Expr>,
    pub pipeline: Vec<PipelineOp>,
    pub bind: Option<Binding>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub name: Name,
    pub ty: Option<TypeExpr>,
}

#[derive(Clone, Debug)]
pub enum PipelineOp {
    Filter {
        lambda: Expr,
        span: Span,
    },
    Map {
        lambda: Expr,
        span: Span,
    },
    Sort {
        by: Expr,
        desc: bool,
        span: Span,
    },
    Limit {
        count: Expr,
        span: Span,
    },
    Skip {
        count: Expr,
        span: Span,
    },
    Reverse {
        span: Span,
    },
    /// Deduplicate, optionally by a key expression.
    Unique {
        by: Option<Expr>,
        span: Span,
    },
    /// Flatten one level of nested arrays.
    Flatten {
        span: Span,
    },
    /// Terminal: the number of elements (an `Integer`).
    Count {
        span: Span,
    },
    /// Terminal: the sum of the elements (or of `by` over each element).
    Sum {
        by: Option<Expr>,
        span: Span,
    },
}

impl PipelineOp {
    pub fn span(&self) -> Span {
        match self {
            PipelineOp::Filter { span, .. }
            | PipelineOp::Map { span, .. }
            | PipelineOp::Sort { span, .. }
            | PipelineOp::Limit { span, .. }
            | PipelineOp::Skip { span, .. }
            | PipelineOp::Reverse { span }
            | PipelineOp::Unique { span, .. }
            | PipelineOp::Flatten { span }
            | PipelineOp::Count { span }
            | PipelineOp::Sum { span, .. } => *span,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            PipelineOp::Filter { .. } => "filter",
            PipelineOp::Map { .. } => "map",
            PipelineOp::Sort { .. } => "sort",
            PipelineOp::Limit { .. } => "limit",
            PipelineOp::Skip { .. } => "skip",
            PipelineOp::Reverse { .. } => "reverse",
            PipelineOp::Unique { .. } => "unique",
            PipelineOp::Flatten { .. } => "flatten",
            PipelineOp::Count { .. } => "count",
            PipelineOp::Sum { .. } => "sum",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PathPattern {
    pub segments: Vec<PathSeg>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PathSeg {
    Literal(String),
    /// `{expr}` — substituted from a value in scope at runtime, e.g. `{username}`
    /// or `{user.company}`.
    Param(Expr),
}

// ---------- expressions ----------

#[derive(Clone, Debug)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Bool(bool, Span),
    Null(Span),
    Duration(u64, Span),
    Str {
        parts: Vec<StrPart>,
        span: Span,
    },
    Ident(Name),
    EnvVar(Name),
    /// A field access whose receiver is the implicit pipeline element, e.g. `.stars`.
    ImplicitField(Name),
    Field {
        base: Box<Expr>,
        field: Name,
        span: Span,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        span: Span,
    },
    Unary {
        op: UnOp,
        rhs: Box<Expr>,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Record {
        name: Option<Name>,
        fields: Vec<(Name, Expr)>,
        span: Span,
    },
    Array {
        elems: Vec<Expr>,
        span: Span,
    },
    /// `...expr`, only valid inside array literals.
    Spread {
        expr: Box<Expr>,
        span: Span,
    },
    Lambda {
        param: Name,
        body: Box<Expr>,
        span: Span,
    },
    Match(Box<MatchExpr>),
    Range {
        lo: Box<Expr>,
        hi: Box<Expr>,
        span: Span,
    },
}

#[derive(Clone, Debug)]
pub enum StrPart {
    Lit(String),
    Interp(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "and",
            BinOp::Or => "or",
        }
    }
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        )
    }
}

#[derive(Clone, Debug)]
pub struct MatchExpr {
    pub scrutinee: Expr,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: ArmBody,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Pattern {
    Wildcard(Span),
    Binding(Name),
    /// `Ok(x)`, `Err(NotFound)`, `Err(RateLimit(ms))`.
    Ctor {
        name: Name,
        args: Vec<Pattern>,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard(s) => *s,
            Pattern::Binding(n) => n.span,
            Pattern::Ctor { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug)]
pub enum ArmBody {
    Value(Expr),
    Block(Block),
    /// `... then retry` — run the effects (calls / `wait`) then retry the fetch.
    Retry {
        effects: Vec<Effect>,
        span: Span,
    },
}

#[derive(Clone, Debug)]
pub enum Effect {
    Call(Expr),
    Wait(Expr),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Bool(_, s)
            | Expr::Null(s)
            | Expr::Duration(_, s) => *s,
            Expr::Str { span, .. } => *span,
            Expr::Ident(n) | Expr::EnvVar(n) | Expr::ImplicitField(n) => n.span,
            Expr::Field { span, .. }
            | Expr::Call { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Record { span, .. }
            | Expr::Array { span, .. }
            | Expr::Spread { span, .. }
            | Expr::Lambda { span, .. }
            | Expr::Range { span, .. } => *span,
            Expr::Match(m) => m.span,
        }
    }
}
