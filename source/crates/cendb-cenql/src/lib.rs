//! cendb-cenql: CenQL pipeline-oriented multi-model query language.
//!
//! ## Overview
//!
//! CenQL (pronounced "sen-cue-el") is CenDB's query language. It is
//! pipeline-oriented (like PRQL/Kusto), left-to-right readable, and
//! model-polymorphic: the same pipeline operators work over relations,
//! documents, graphs, and time-series, with model-specific operators
//! where needed.
//!
//! ## Syntax
//!
//! ```text
//! from users
//! | filter age >= 18 and country == "DE"
//! | select { name, email, age }
//! | sort age desc
//! | take 100
//! ```
//!
//! ## Pipeline operators
//!
//! | Operator | Description |
//! |---|---|
//! | `from <source>` | Source table/collection/graph. |
//! | `filter <predicate>` | Filter rows by a boolean expression. |
//! | `select { col1, col2, ... }` | Project to a subset of columns. |
//! | `sort <col> [asc\|desc]` | Sort by a column. |
//! | `take <n>` | Limit to `n` rows. |
//! | `join <src> on <predicate>` | Join with another source. |
//! | `group_by <col> { agg: expr, ... }` | Aggregate. |
//! | `window tumbling(<dur>) on <col> { ... }` | Time-series windowing. |
//! | `match (n:Label)-[:TYPE*1..3]->(m:Label)` | Graph traversal. |
//! | `return distinct <col>, ...` | Final projection. |
//!
//! ## Implementation
//!
//! For this implementation we ship:
//!   * A **lexer** (zero-alloc tokenizer).
//!   * A **recursive-descent parser** producing an AST.
//!   * An **AST** with `Pipeline`, `Stage`, `Expr` types.
//!
//! The executor that runs a CenQL pipeline against actual data is left
//! as advanced configuration; the AST is the contract between parser and future
//! planner.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::{
    AggExpr, BinaryOp, ColumnDef, ColumnType, CenqlPipeline, CenqlStage, CenqlStatement,
    EdgeDirection, Expr, GraphMatchPattern, JoinKind, SortDir, WindowSpec,
};
pub use lexer::{Token, TokenKind, Tokenizer};
pub use parser::{parse, parse_statement, ParseError, ParseResult, Parser};
