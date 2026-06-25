//! `tired-syntax` — the front of the TIRED compiler: source spans, a hand-written
//! lexer, a recursive-descent parser, the AST, and a `rustc`-style diagnostic
//! renderer. This crate is intentionally **dependency-free**: nothing here touches
//! the network or pulls in third-party code.
//!
//! ```
//! let (program, diags) = tired_syntax::parse("fetch GitHub /users/gabriel -> user");
//! assert!(!diags.has_errors());
//! assert_eq!(program.items.len(), 1);
//! ```

pub mod ast;
pub mod diag;
pub mod lexer;
pub mod parser;
pub mod pretty;
pub mod span;
pub mod token;

pub use diag::{Diagnostic, Diagnostics, Severity};
pub use parser::parse;
pub use span::{Span, Spanned};
