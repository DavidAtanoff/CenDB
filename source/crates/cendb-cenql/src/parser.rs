//! CenQL recursive-descent parser.

use std::fmt;

use crate::ast::*;
use crate::lexer::{LexError, Token, TokenKind, Tokenizer};

// ============================================================================
// Errors.
// ============================================================================

#[derive(Debug, Clone)]
pub enum ParseError {
    Lex(LexError),
    Unexpected {
        expected: &'static str,
        got: TokenKind,
        offset: usize,
    },
    UnexpectedEof,
    Other(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Lex(e) => write!(f, "{}", e),
            ParseError::Unexpected { expected, got, offset } => {
                write!(f, "parse error at offset {}: expected {}, got {:?}", offset, expected, got)
            }
            ParseError::UnexpectedEof => write!(f, "parse error: unexpected end of input"),
            ParseError::Other(s) => write!(f, "parse error: {}", s),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError::Lex(e)
    }
}

pub type ParseResult<T> = Result<T, ParseError>;

// ============================================================================
// Parser.
// ============================================================================

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(src: &str) -> ParseResult<Self> {
        let tokens = Tokenizer::new(src).tokenize()?;
        Ok(Self { tokens, pos: 0 })
    }

    /// Parse a full CenQL pipeline.
    pub fn parse_pipeline(&mut self) -> ParseResult<CenqlPipeline> {
        let mut stages = Vec::new();
        // First stage must be `from <source>`.
        stages.push(self.parse_from()?);
        // Subsequent stages are introduced by `|`.
        while self.peek_kind() == Some(TokenKind::Pipe) {
            self.advance(); // consume `|`
            stages.push(self.parse_stage()?);
        }
        // Don't enforce EOF here — the caller (parse_top_level or
        // parse_statement) may need to check for set operations after
        // the pipeline. If called directly via `parse()`, the caller
        // expects EOF, so we check there.
        Ok(CenqlPipeline::new(stages))
    }

    fn parse_from(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::From)?;
        let mut name = self.expect_ident()?;
        // Allow two-word source names like `graph social` (used by the
        // graph model). We consume a second identifier if it follows,
        // UNLESS it's a set-operation keyword (union/intersect/except).
        if self.peek_kind() == Some(TokenKind::Ident) {
            if let Some(tok) = self.peek_token() {
                let lower = tok.text.to_lowercase();
                if lower != "union" && lower != "intersect" && lower != "except" {
                    let second = self.advance().text.clone();
                    name.push(' ');
                    name.push_str(&second);
                }
            }
        }
        Ok(CenqlStage::From { name })
    }

    fn parse_stage(&mut self) -> ParseResult<CenqlStage> {
        match self.peek_kind() {
            Some(TokenKind::Filter) => self.parse_filter(),
            Some(TokenKind::Select) => self.parse_select(),
            Some(TokenKind::Sort) => self.parse_sort(),
            Some(TokenKind::Take) => self.parse_take(),
            Some(TokenKind::Join) => self.parse_join(),
            Some(TokenKind::GroupBy) => self.parse_group_by(),
            Some(TokenKind::Window) => self.parse_window(),
            Some(TokenKind::Match) => self.parse_match(),
            Some(TokenKind::Return) => self.parse_return(),
            Some(k) => Err(self.unexpected_kind(k, "pipeline stage")),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn parse_filter(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Filter)?;
        let expr = self.parse_expr()?;
        Ok(CenqlStage::Filter { expr })
    }

    fn parse_select(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Select)?;
        self.expect(TokenKind::LBrace)?;
        let mut columns = Vec::new();
        loop {
            let name = self.expect_ident_or_path()?;
            columns.push(name);
            if self.peek_kind() == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(CenqlStage::Select { columns })
    }

    fn parse_sort(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Sort)?;
        let column = self.expect_ident_or_path()?;
        let dir = match self.peek_kind() {
            Some(TokenKind::Asc) => {
                self.advance();
                SortDir::Asc
            }
            Some(TokenKind::Desc) => {
                self.advance();
                SortDir::Desc
            }
            _ => SortDir::Asc,
        };
        Ok(CenqlStage::Sort { column, dir })
    }

    fn parse_take(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Take)?;
        let n = self.parse_u64()?;
        Ok(CenqlStage::Take { n })
    }

    fn parse_join(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Join)?;
        // Optional kind: `inner`, `left`, `right`, `full`.
        let kind = match self.peek_kind() {
            Some(TokenKind::Inner) => {
                self.advance();
                JoinKind::Inner
            }
            Some(TokenKind::Left) => {
                self.advance();
                JoinKind::Left
            }
            Some(TokenKind::Right) => {
                self.advance();
                JoinKind::Right
            }
            Some(TokenKind::Full) => {
                self.advance();
                JoinKind::Full
            }
            _ => JoinKind::Inner,
        };
        let source = self.expect_ident()?;
        self.expect(TokenKind::On)?;
        let on = self.parse_expr()?;
        Ok(CenqlStage::Join { source, kind, on })
    }

    fn parse_group_by(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::GroupBy)?;
        let key = self.expect_ident_or_path()?;
        self.expect(TokenKind::LBrace)?;
        let mut aggs = Vec::new();
        loop {
            let name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let func = self.expect_ident()?;
            self.expect(TokenKind::LParen)?;
            let args = self.parse_args()?;
            self.expect(TokenKind::RParen)?;
            aggs.push(AggExpr { name, func, args });
            if self.peek_kind() == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(CenqlStage::GroupBy { key, aggs })
    }

    fn parse_window(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Window)?;
        let spec = match self.peek_kind() {
            Some(TokenKind::Tumbling) => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let dur = self.expect_duration()?;
                self.expect(TokenKind::RParen)?;
                WindowSpec::Tumbling(dur)
            }
            Some(TokenKind::Hopping) => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let size = self.expect_duration()?;
                self.expect(TokenKind::Comma)?;
                let slide = self.expect_duration()?;
                self.expect(TokenKind::RParen)?;
                WindowSpec::Hopping { size, slide }
            }
            Some(TokenKind::Session) => {
                self.advance();
                self.expect(TokenKind::LParen)?;
                let gap = self.expect_duration()?;
                self.expect(TokenKind::RParen)?;
                WindowSpec::Session(gap)
            }
            _ => return Err(self.unexpected("window kind (tumbling/hopping/session)")),
        };
        self.expect(TokenKind::On)?;
        let on = self.expect_ident_or_path()?;
        self.expect(TokenKind::LBrace)?;
        let mut aggs = Vec::new();
        loop {
            let name = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let func = self.expect_ident()?;
            self.expect(TokenKind::LParen)?;
            let args = self.parse_args()?;
            self.expect(TokenKind::RParen)?;
            aggs.push(AggExpr { name, func, args });
            if self.peek_kind() == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(TokenKind::RBrace)?;
        Ok(CenqlStage::Window { spec, on, aggs })
    }

    fn parse_match(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Match)?;
        // (var:Label) -[:TYPE*min..max]-> (var:Label)
        self.expect(TokenKind::LParen)?;
        let start_var = self.expect_ident()?;
        let start_label = if self.peek_kind() == Some(TokenKind::Colon) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(TokenKind::RParen)?;

        // Edge: `-[:TYPE*min..max]->` or `<-[:TYPE]-`.
        let edge_direction = match self.peek_kind() {
            Some(TokenKind::Minus) => EdgeDirection::Out,
            Some(TokenKind::BackArrow) => EdgeDirection::In,
            _ => return Err(self.unexpected("edge start ('-' or '<-')")),
        };
        if edge_direction == EdgeDirection::In {
            self.advance(); // consume `<-`
        } else {
            self.advance(); // consume `-`
        }
        self.expect(TokenKind::LBracket)?;
        let edge_type = if self.peek_kind() == Some(TokenKind::Colon) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        let mut edge_min_hops = 1u32;
        let mut edge_max_hops = 1u32;
        if self.peek_kind() == Some(TokenKind::Star) {
            self.advance();
            // Optional `min..max`.
            if self.peek_kind() == Some(TokenKind::I64) {
                edge_min_hops = self.parse_u32()?;
                if self.peek_kind() == Some(TokenKind::Dot) {
                    self.advance();
                    if self.peek_kind() == Some(TokenKind::Dot) {
                        self.advance();
                    }
                    edge_max_hops = self.parse_u32()?;
                }
            }
        }
        self.expect(TokenKind::RBracket)?;
        if edge_direction == EdgeDirection::Out {
            self.expect(TokenKind::Arrow)?;
        } else {
            self.expect(TokenKind::Minus)?;
        }

        self.expect(TokenKind::LParen)?;
        let end_var = self.expect_ident()?;
        let end_label = if self.peek_kind() == Some(TokenKind::Colon) {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(TokenKind::RParen)?;

        Ok(CenqlStage::Match {
            pattern: GraphMatchPattern {
                start_var,
                start_label,
                edge_type,
                edge_min_hops,
                edge_max_hops,
                edge_direction,
                end_var,
                end_label,
            },
        })
    }

    fn parse_return(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::Return)?;
        let distinct = if self.peek_kind() == Some(TokenKind::Distinct) {
            self.advance();
            true
        } else {
            false
        };
        let mut columns = Vec::new();
        loop {
            let name = self.expect_ident_or_path()?;
            columns.push(name);
            if self.peek_kind() == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(CenqlStage::Return { distinct, columns })
    }

    // ========================================================================
    // Expression parser (recursive descent with precedence).
    // ========================================================================

    fn parse_expr(&mut self) -> ParseResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> ParseResult<Expr> {
        let mut lhs = self.parse_and()?;
        while self.peek_kind() == Some(TokenKind::Or) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary {
                op: BinaryOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> ParseResult<Expr> {
        let mut lhs = self.parse_comparison()?;
        while self.peek_kind() == Some(TokenKind::And) {
            self.advance();
            let rhs = self.parse_comparison()?;
            lhs = Expr::Binary {
                op: BinaryOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> ParseResult<Expr> {
        let lhs = self.parse_additive()?;
        let op = match self.peek_kind() {
            Some(TokenKind::EqEq) => BinaryOp::Eq,
            Some(TokenKind::NotEq) => BinaryOp::Ne,
            Some(TokenKind::Lt) => BinaryOp::Lt,
            Some(TokenKind::Le) => BinaryOp::Le,
            Some(TokenKind::Gt) => BinaryOp::Gt,
            Some(TokenKind::Ge) => BinaryOp::Ge,
            _ => return Ok(lhs),
        };
        self.advance();
        let rhs = self.parse_additive()?;
        Ok(Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    }

    fn parse_additive(&mut self) -> ParseResult<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Plus) => BinaryOp::Add,
                Some(TokenKind::Minus) => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> ParseResult<Expr> {
        let mut lhs = self.parse_primary()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Star) => BinaryOp::Mul,
                Some(TokenKind::Slash) => BinaryOp::Div,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_primary()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_primary(&mut self) -> ParseResult<Expr> {
        match self.peek_kind() {
            Some(TokenKind::I64) => {
                let text = self.advance().text.clone();
                let v: i64 = text.parse().map_err(|_| ParseError::Other(format!("invalid i64: {}", text)))?;
                Ok(Expr::I64(v))
            }
            Some(TokenKind::F64) => {
                let text = self.advance().text.clone();
                let v: f64 = text.parse().map_err(|_| ParseError::Other(format!("invalid f64: {}", text)))?;
                Ok(Expr::F64(v))
            }
            Some(TokenKind::Str) => {
                let text = self.advance().text.clone();
                Ok(Expr::Str(text))
            }
            Some(TokenKind::Ident) => {
                // Could be a column or a function call.
                let name = self.advance().text.clone();
                if self.peek_kind() == Some(TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if self.peek_kind() != Some(TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.peek_kind() == Some(TokenKind::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen)?;
                    Ok(Expr::Call { name, args })
                } else {
                    // Maybe a dotted path: user.address.city
                    let mut full = name;
                    while self.peek_kind() == Some(TokenKind::Dot) {
                        self.advance();
                        let next = self.expect_ident()?;
                        full.push('.');
                        full.push_str(&next);
                    }
                    Ok(Expr::Column(full))
                }
            }
            Some(TokenKind::LParen) => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect(TokenKind::RParen)?;
                Ok(e)
            }
            Some(k) => Err(self.unexpected_kind(k, "expression")),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    // ========================================================================
    // Helpers.
    // ========================================================================

    /// Parse a comma-separated list of arguments (possibly empty).
    fn parse_args(&mut self) -> ParseResult<Vec<Expr>> {
        let mut args = Vec::new();
        if self.peek_kind() == Some(TokenKind::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if self.peek_kind() == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        Ok(args)
    }

    fn peek_kind(&self) -> Option<TokenKind> {
        self.tokens.get(self.pos).map(|t| t.kind)
    }

    /// Peek at the current token (returns a reference to the full Token).
    fn peek_token(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos];
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, kind: TokenKind) -> ParseResult<()> {
        match self.peek_kind() {
            Some(k) if k == kind => {
                self.advance();
                Ok(())
            }
            Some(k) => Err(self.unexpected_kind(k, "specific token")),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn expect_ident(&mut self) -> ParseResult<String> {
        match self.peek_kind() {
            Some(TokenKind::Ident) => {
                let text = self.advance().text.clone();
                Ok(text)
            }
            Some(k) => Err(self.unexpected_kind(k, "identifier")),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn expect_ident_or_path(&mut self) -> ParseResult<String> {
        let first = self.expect_ident()?;
        let mut full = first;
        while self.peek_kind() == Some(TokenKind::Dot) {
            self.advance();
            let next = self.expect_ident()?;
            full.push('.');
            full.push_str(&next);
        }
        Ok(full)
    }

    fn expect_duration(&mut self) -> ParseResult<String> {
        match self.peek_kind() {
            Some(TokenKind::Duration) => Ok(self.advance().text.clone()),
            Some(k) => Err(self.unexpected_kind(k, "duration (e.g. 5m, 1h, 30s)")),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn parse_u64(&mut self) -> ParseResult<u64> {
        match self.peek_kind() {
            Some(TokenKind::I64) => {
                let text = self.advance().text.clone();
                text.parse::<u64>()
                    .map_err(|_| ParseError::Other(format!("invalid u64: {}", text)))
            }
            Some(k) => Err(self.unexpected_kind(k, "integer literal")),
            None => Err(ParseError::UnexpectedEof),
        }
    }

    fn parse_u32(&mut self) -> ParseResult<u32> {
        let v = self.parse_u64()?;
        v.try_into()
            .map_err(|_| ParseError::Other(format!("u32 overflow: {}", v)))
    }

    fn unexpected(&self, expected: &'static str) -> ParseError {
        match self.tokens.get(self.pos) {
            Some(t) => ParseError::Unexpected {
                expected,
                got: t.kind,
                offset: t.offset,
            },
            None => ParseError::UnexpectedEof,
        }
    }

    fn unexpected_kind(&self, got: TokenKind, expected: &'static str) -> ParseError {
        match self.tokens.get(self.pos) {
            Some(t) => ParseError::Unexpected {
                expected,
                got,
                offset: t.offset,
            },
            None => ParseError::UnexpectedEof,
        }
    }
}

/// Convenience: parse a CenQL source string into a pipeline.
pub fn parse(src: &str) -> ParseResult<CenqlPipeline> {
    let mut parser = Parser::new(src)?;
    let pipeline = parser.parse_pipeline()?;
    // Enforce EOF for the convenience function.
    if parser.peek_kind() != Some(TokenKind::Eof) {
        return Err(parser.unexpected("end of pipeline"));
    }
    Ok(pipeline)
}

/// Parse a CenQL source string into a top-level statement (query, DDL,
/// DML, or transaction control).
pub fn parse_statement(src: &str) -> ParseResult<CenqlStatement> {
    let mut parser = Parser::new(src)?;
    parser.parse_top_level()
}

impl Parser {
    /// Parse a top-level CenQL statement.
    pub fn parse_top_level(&mut self) -> ParseResult<CenqlStatement> {
        // Peek at the first token to determine statement type.
        let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;

        match tok.kind {
            TokenKind::Ident => {
                let kw = tok.text.to_lowercase();
                match kw.as_str() {
                    "create" => self.parse_create(),
                    "drop" => self.parse_drop(),
                    "insert" => self.parse_insert(),
                    "update" => self.parse_update(),
                    "delete" => self.parse_delete_stmt(),
                    "upsert" => self.parse_upsert(),
                    "begin" => {
                        self.advance();
                        Ok(CenqlStatement::Begin)
                    }
                    "commit" => {
                        self.advance();
                        Ok(CenqlStatement::Commit)
                    }
                    "rollback" => {
                        self.advance();
                        Ok(CenqlStatement::Rollback)
                    }
                    "with" => self.parse_cte(),
                    _ => {
                        // Unknown ident — try as a query pipeline.
                        let pipeline = self.parse_pipeline()?;
                        // Check for set operations (union/intersect/except).
                        let stmt = self.check_set_operations(pipeline)?;
                        Ok(stmt)
                    }
                }
            }
            TokenKind::Distinct => {
                self.advance();
                let pipeline = self.parse_pipeline()?;
                Ok(CenqlStatement::Distinct {
                    pipeline: Box::new(pipeline),
                })
            }
            TokenKind::From => {
                // A query pipeline starting with `from`.
                let pipeline = self.parse_pipeline()?;
                // Check for set operations (union/intersect/except).
                let stmt = self.check_set_operations(pipeline)?;
                Ok(stmt)
            }
            _ => {
                // Parse as a query pipeline.
                let pipeline = self.parse_pipeline()?;
                Ok(CenqlStatement::Query(pipeline))
            }
        }
    }

    /// Check if the next token is a set operation (union/intersect/except).
    fn check_set_operations(&mut self, left: CenqlPipeline) -> ParseResult<CenqlStatement> {
        if let Some(next_tok) = self.peek_token() {
            if next_tok.kind == TokenKind::Ident {
                let next_kw = next_tok.text.to_lowercase();
                if next_kw == "union" {
                    self.advance();
                    let all = if let Some(t) = self.peek_token() {
                        t.kind == TokenKind::Ident && t.text.to_lowercase() == "all"
                    } else { false };
                    if all { self.advance(); }
                    let right = self.parse_pipeline()?;
                    return Ok(CenqlStatement::Union {
                        left: Box::new(left),
                        right: Box::new(right),
                        all,
                    });
                }
                if next_kw == "intersect" {
                    self.advance();
                    let right = self.parse_pipeline()?;
                    return Ok(CenqlStatement::Intersect {
                        left: Box::new(left),
                        right: Box::new(right),
                    });
                }
                if next_kw == "except" {
                    self.advance();
                    let right = self.parse_pipeline()?;
                    return Ok(CenqlStatement::Except {
                        left: Box::new(left),
                        right: Box::new(right),
                    });
                }
            }
        }
        Ok(CenqlStatement::Query(left))
    }

    /// Parse a CREATE statement (table, index, or view).
    fn parse_create(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "create"
        let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;
        if tok.kind == TokenKind::Ident {
            let kw = tok.text.to_lowercase(); match kw.as_str() {
                "table" => self.parse_create_table(),
                "index" => self.parse_create_index(),
                "view" => self.parse_create_view(),
                _ => Err(ParseError::Other(format!("expected table/index/view after create, got {}", kw))),
            }
        } else {
            Err(ParseError::Other("expected table/index/view after create".into()))
        }
    }

    fn parse_create_table(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "table"
        let name = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut columns = Vec::new();
        let mut primary_key = None;
        loop {
            let col_name = self.expect_ident()?;
            // Check for "primary_key" special column.
            if col_name.to_lowercase() == "primary_key" {
                self.expect(TokenKind::Colon)?;
                primary_key = Some(self.expect_ident()?);
            } else {
                self.expect(TokenKind::Colon)?;
                let type_tok = self.expect_ident()?;
                let data_type = match type_tok.to_lowercase().as_str() {
                    "i64" | "int" | "integer" => ColumnType::I64,
                    "f64" | "float" | "double" => ColumnType::F64,
                    "str" | "string" | "text" => ColumnType::Str,
                    "bool" | "boolean" => ColumnType::Bool,
                    "bytes" | "blob" => ColumnType::Bytes,
                    "timestamp" | "time" => ColumnType::Timestamp,
                    "json" => ColumnType::Json,
                    _ => ColumnType::Str, // default to string
                };
                // Check for "not null".
                let nullable = if let Some(t) = self.peek_token() {
                    if t.kind == TokenKind::Not {
                        self.advance();
                        // Expect "null" — but "null" might be tokenized as
                        // an Ident since it's not in the keyword list.
                        if let Some(t2) = self.peek_token() {
                            if t2.kind == TokenKind::Ident && t2.text.to_lowercase() == "null" {
                                self.advance();
                                false
                            } else { true }
                        } else { true }
                    } else { true }
                } else { true };
                columns.push(ColumnDef { name: col_name, data_type, nullable });
            }
            // Check for comma or close brace.
            let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;
            match tok.kind {
                TokenKind::Comma => { self.advance(); }
                TokenKind::RBrace => { self.advance(); break; }
                _ => break,
            }
        }
        Ok(CenqlStatement::CreateTable { name, columns, primary_key })
    }

    fn parse_create_index(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "index"
        let name = self.expect_ident()?;
        // Expect "on" — this is a keyword, so we expect TokenKind::On.
        self.expect(TokenKind::On)?;
        let table = self.expect_ident()?;
        self.expect(TokenKind::LParen)?;
        let column = self.expect_ident()?;
        self.expect(TokenKind::RParen)?;
        Ok(CenqlStatement::CreateIndex { name, table, column })
    }

    fn parse_create_view(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "view"
        let name = self.expect_ident()?;
        // Expect "as"
        let as_tok = self.expect_ident()?;
        if as_tok.to_lowercase() != "as" {
            return Err(ParseError::Other(format!("expected 'as' after view name, got {}", as_tok)));
        }
        let pipeline = self.parse_pipeline()?;
        Ok(CenqlStatement::CreateView {
            name,
            pipeline: Box::new(pipeline),
        })
    }

    /// Parse a DROP statement.
    fn parse_drop(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "drop"
        let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;
        if tok.kind == TokenKind::Ident {
            let kw = tok.text.to_lowercase(); match kw.as_str() {
                "table" => {
                    self.advance();
                    let name = self.expect_ident()?;
                    Ok(CenqlStatement::DropTable { name })
                }
                "index" => {
                    self.advance();
                    let name = self.expect_ident()?;
                    Ok(CenqlStatement::DropIndex { name })
                }
                "view" => {
                    self.advance();
                    let name = self.expect_ident()?;
                    Ok(CenqlStatement::DropView { name })
                }
                _ => Err(ParseError::Other(format!("expected table/index/view after drop, got {}", kw))),
            }
        } else {
            Err(ParseError::Other("expected table/index/view after drop".into()))
        }
    }

    /// Parse an INSERT statement.
    fn parse_insert(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "insert"
        // Expect "into"
        let into_tok = self.expect_ident()?;
        if into_tok.to_lowercase() != "into" {
            return Err(ParseError::Other(format!("expected 'into' after insert, got {}", into_tok)));
        }
        let table = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut values = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let val = self.parse_expr()?;
            values.push((col, val));
            let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;
            match tok.kind {
                TokenKind::Comma => { self.advance(); }
                TokenKind::RBrace => { self.advance(); break; }
                _ => break,
            }
        }
        Ok(CenqlStatement::Insert { table, values })
    }

    /// Parse an UPDATE statement.
    fn parse_update(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "update"
        let table = self.expect_ident()?;
        // Expect "set"
        let set_tok = self.expect_ident()?;
        if set_tok.to_lowercase() != "set" {
            return Err(ParseError::Other(format!("expected 'set' after table name, got {}", set_tok)));
        }
        let mut assignments = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(TokenKind::Assign)?;
            let val = self.parse_expr()?;
            assignments.push((col, val));
            let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;
            match tok.kind {
                TokenKind::Comma => { self.advance(); }
                _ => break,
            }
        }
        // Check for WHERE.
        let where_clause = if let Some(t) = self.peek_token() {
            if t.kind == TokenKind::Ident {
                if t.text.to_lowercase() == "where" {
                    self.advance();
                    Some(self.parse_expr()?)
                } else { None }
            } else { None }
        } else { None };
        Ok(CenqlStatement::Update { table, assignments, where_clause })
    }

    /// Parse a DELETE statement.
    fn parse_delete_stmt(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "delete"
        // Expect "from" — this is a keyword.
        self.expect(TokenKind::From)?;
        let table = self.expect_ident()?;
        // Check for WHERE.
        let where_clause = if let Some(t) = self.peek_token() {
            if t.kind == TokenKind::Ident && t.text.to_lowercase() == "where" {
                self.advance();
                Some(self.parse_expr()?)
            } else { None }
        } else { None };
        Ok(CenqlStatement::Delete { table, where_clause })
    }

    /// Parse an UPSERT statement.
    fn parse_upsert(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "upsert"
        // Expect "into"
        let into_tok = self.expect_ident()?;
        if into_tok.to_lowercase() != "into" {
            return Err(ParseError::Other(format!("expected 'into' after upsert, got {}", into_tok)));
        }
        let table = self.expect_ident()?;
        self.expect(TokenKind::LBrace)?;
        let mut values = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(TokenKind::Colon)?;
            let val = self.parse_expr()?;
            values.push((col, val));
            let tok = self.peek_token().ok_or(ParseError::UnexpectedEof)?;
            match tok.kind {
                TokenKind::Comma => { self.advance(); }
                TokenKind::RBrace => { self.advance(); break; }
                _ => break,
            }
        }
        Ok(CenqlStatement::Upsert { table, values })
    }

    /// Parse a CTE (WITH ... AS (...) ...).
    fn parse_cte(&mut self) -> ParseResult<CenqlStatement> {
        self.advance(); // consume "with"
        let cte_name = self.expect_ident()?;
        // Expect "as"
        let as_tok = self.expect_ident()?;
        if as_tok.to_lowercase() != "as" {
            return Err(ParseError::Other(format!("expected 'as' after CTE name, got {}", as_tok)));
        }
        self.expect(TokenKind::LParen)?;
        let cte_pipeline = self.parse_pipeline()?;
        self.expect(TokenKind::RParen)?;
        let main_pipeline = self.parse_pipeline()?;
        Ok(CenqlStatement::WithCte {
            cte_name,
            cte_pipeline: Box::new(cte_pipeline),
            main_pipeline: Box::new(main_pipeline),
        })
    }
}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_pipeline() {
        let p = parse(
            r#"from users
               | filter age >= 18 and country == "DE"
               | select { name, email, age }
               | sort age desc
               | take 100"#,
        )
        .unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(p.source(), Some("users"));
    }

    #[test]
    fn parse_join() {
        let p = parse(
            r#"from orders
               | join customers on orders.customer_id == customers.id
               | take 10"#,
        )
        .unwrap();
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn parse_group_by() {
        let p = parse(
            r#"from orders
               | group_by region {
                   revenue: sum(total),
                   count: count()
                 }"#,
        )
        .unwrap();
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn parse_window() {
        let p = parse(
            r#"from metrics
               | window tumbling(5m) on ts {
                   avg: mean(temperature),
                   p99: percentile(temperature, 99)
                 }"#,
        )
        .unwrap();
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn parse_graph_match() {
        let p = parse(
            r#"from graph social
               | match (a:Person)-[:FOLLOWS*1..3]->(b:Person)"#,
        )
        .unwrap();
        assert_eq!(p.len(), 2);
        if let CenqlStage::Match { pattern } = &p.stages[1] {
            assert_eq!(pattern.start_var, "a");
            assert_eq!(pattern.start_label.as_deref(), Some("Person"));
            assert_eq!(pattern.edge_type.as_deref(), Some("FOLLOWS"));
            assert_eq!(pattern.edge_min_hops, 1);
            assert_eq!(pattern.edge_max_hops, 3);
            assert_eq!(pattern.edge_direction, EdgeDirection::Out);
            assert_eq!(pattern.end_var, "b");
        } else {
            panic!("expected Match stage");
        }
    }

    #[test]
    fn parse_return_distinct() {
        let p = parse(r#"from users | return distinct name, email"#).unwrap();
        assert_eq!(p.len(), 2);
        if let CenqlStage::Return { distinct, columns } = &p.stages[1] {
            assert!(*distinct);
            assert_eq!(columns, &["name", "email"]);
        } else {
            panic!("expected Return stage");
        }
    }

    #[test]
    fn parse_dotted_path_in_filter() {
        let p = parse(r#"from events | filter payload.user.tier == "premium""#).unwrap();
        if let CenqlStage::Filter { expr } = &p.stages[1] {
            if let Expr::Binary { lhs, .. } = expr {
                if let Expr::Column(c) = lhs.as_ref() {
                    assert_eq!(c, "payload.user.tier");
                } else {
                    panic!("expected Column");
                }
            }
        } else {
            panic!("expected Filter");
        }
    }

    #[test]
    fn parse_complex_expression() {
        let p = parse(r#"from orders | filter (price * quantity) > 100 and tax < 10"#).unwrap();
        if let CenqlStage::Filter { expr } = &p.stages[1] {
            // Top-level should be an And.
            assert!(matches!(expr, Expr::Binary { op: BinaryOp::And, .. }));
        }
    }

    #[test]
    fn parse_error_on_unexpected_token() {
        let result = parse(r#"from users | bogus"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_error_on_missing_pipe() {
        let result = parse(r#"from users filter age > 5"#);
        assert!(result.is_err());
    }

    #[test]
    fn pipeline_display_roundtrip_simple() {
        let src = r#"from users | take 5"#;
        let p = parse(src).unwrap();
        let displayed = format!("{}", p);
        // The displayed form should still parse.
        let p2 = parse(&displayed).unwrap();
        assert_eq!(p.len(), p2.len());
    }

    // ========================================================================
    // DDL statement tests.
    // ========================================================================

    #[test]
    fn parse_create_table() {
        let stmt = parse_statement(r#"create table users { id: i64 not null, name: str, email: str not null, primary_key: id }"#).unwrap();
        match stmt {
            CenqlStatement::CreateTable { name, columns, primary_key } => {
                assert_eq!(name, "users");
                // primary_key is parsed as a special directive, not a column
                assert_eq!(columns.len(), 3);
                assert_eq!(columns[0].name, "id");
                assert_eq!(columns[0].data_type, ColumnType::I64);
                assert!(!columns[0].nullable);
                assert_eq!(columns[1].name, "name");
                assert_eq!(columns[1].data_type, ColumnType::Str);
                assert!(columns[1].nullable);
                assert_eq!(columns[2].name, "email");
                assert!(!columns[2].nullable);
                assert_eq!(primary_key, Some("id".to_string()));
            }
            _ => panic!("expected CreateTable, got {:?}", stmt),
        }
    }

    #[test]
    fn parse_create_table_all_types() {
        let stmt = parse_statement(r#"create table mixed { a: i64, b: f64, c: str, d: bool, e: bytes, f: timestamp, g: json }"#).unwrap();
        match stmt {
            CenqlStatement::CreateTable { columns, .. } => {
                assert_eq!(columns.len(), 7);
                assert_eq!(columns[0].data_type, ColumnType::I64);
                assert_eq!(columns[1].data_type, ColumnType::F64);
                assert_eq!(columns[2].data_type, ColumnType::Str);
                assert_eq!(columns[3].data_type, ColumnType::Bool);
                assert_eq!(columns[4].data_type, ColumnType::Bytes);
                assert_eq!(columns[5].data_type, ColumnType::Timestamp);
                assert_eq!(columns[6].data_type, ColumnType::Json);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parse_create_table_no_primary_key() {
        let stmt = parse_statement(r#"create table logs { level: str, message: str }"#).unwrap();
        match stmt {
            CenqlStatement::CreateTable { name, columns, primary_key } => {
                assert_eq!(name, "logs");
                assert_eq!(columns.len(), 2);
                assert_eq!(primary_key, None);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parse_drop_table() {
        let stmt = parse_statement("drop table users").unwrap();
        match stmt {
            CenqlStatement::DropTable { name } => assert_eq!(name, "users"),
            _ => panic!("expected DropTable"),
        }
    }

    #[test]
    fn parse_create_index() {
        let stmt = parse_statement("create index idx_name on users(name)").unwrap();
        match stmt {
            CenqlStatement::CreateIndex { name, table, column } => {
                assert_eq!(name, "idx_name");
                assert_eq!(table, "users");
                assert_eq!(column, "name");
            }
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn parse_drop_index() {
        let stmt = parse_statement("drop index idx_name").unwrap();
        match stmt {
            CenqlStatement::DropIndex { name } => assert_eq!(name, "idx_name"),
            _ => panic!("expected DropIndex"),
        }
    }

    #[test]
    fn parse_create_view() {
        let stmt = parse_statement(r#"create view active_users as from users | filter status == "active""#).unwrap();
        match stmt {
            CenqlStatement::CreateView { name, pipeline } => {
                assert_eq!(name, "active_users");
                assert!(pipeline.len() >= 2);
            }
            _ => panic!("expected CreateView"),
        }
    }

    #[test]
    fn parse_drop_view() {
        let stmt = parse_statement("drop view active_users").unwrap();
        match stmt {
            CenqlStatement::DropView { name } => assert_eq!(name, "active_users"),
            _ => panic!("expected DropView"),
        }
    }

    // ========================================================================
    // DML statement tests.
    // ========================================================================

    #[test]
    fn parse_insert() {
        let stmt = parse_statement(r#"insert into users { id: 42, name: "Alice", email: "alice@example.com" }"#).unwrap();
        match stmt {
            CenqlStatement::Insert { table, values } => {
                assert_eq!(table, "users");
                assert_eq!(values.len(), 3);
                assert_eq!(values[0].0, "id");
                assert_eq!(values[1].0, "name");
                assert_eq!(values[2].0, "email");
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_insert_with_expressions() {
        let stmt = parse_statement(r#"insert into products { name: "Widget", price: 9.99, quantity: 100 }"#).unwrap();
        match stmt {
            CenqlStatement::Insert { table, values } => {
                assert_eq!(table, "products");
                assert_eq!(values.len(), 3);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parse_update() {
        let stmt = parse_statement(r#"update users set name = "Bob", age = 30 where id == 42"#).unwrap();
        match stmt {
            CenqlStatement::Update { table, assignments, where_clause } => {
                assert_eq!(table, "users");
                assert_eq!(assignments.len(), 2);
                assert_eq!(assignments[0].0, "name");
                assert_eq!(assignments[1].0, "age");
                assert!(where_clause.is_some());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_update_no_where() {
        let stmt = parse_statement(r#"update config set value = "new_value""#).unwrap();
        match stmt {
            CenqlStatement::Update { table, assignments, where_clause } => {
                assert_eq!(table, "config");
                assert_eq!(assignments.len(), 1);
                assert!(where_clause.is_none());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_delete() {
        let stmt = parse_statement(r#"delete from users where id == 42"#).unwrap();
        match stmt {
            CenqlStatement::Delete { table, where_clause } => {
                assert_eq!(table, "users");
                assert!(where_clause.is_some());
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_delete_no_where() {
        let stmt = parse_statement("delete from temp_table").unwrap();
        match stmt {
            CenqlStatement::Delete { table, where_clause } => {
                assert_eq!(table, "temp_table");
                assert!(where_clause.is_none());
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parse_upsert() {
        let stmt = parse_statement(r#"upsert into users { id: 1, name: "Alice" }"#).unwrap();
        match stmt {
            CenqlStatement::Upsert { table, values } => {
                assert_eq!(table, "users");
                assert_eq!(values.len(), 2);
            }
            _ => panic!("expected Upsert"),
        }
    }

    // ========================================================================
    // Transaction control tests.
    // ========================================================================

    #[test]
    fn parse_begin() {
        let stmt = parse_statement("begin").unwrap();
        assert!(matches!(stmt, CenqlStatement::Begin));
    }

    #[test]
    fn parse_commit() {
        let stmt = parse_statement("commit").unwrap();
        assert!(matches!(stmt, CenqlStatement::Commit));
    }

    #[test]
    fn parse_rollback() {
        let stmt = parse_statement("rollback").unwrap();
        assert!(matches!(stmt, CenqlStatement::Rollback));
    }

    // ========================================================================
    // Set operation tests.
    // ========================================================================

    #[test]
    fn parse_union() {
        let stmt = parse_statement(r#"from users | take 5 union from admins | take 5"#).unwrap();
        match stmt {
            CenqlStatement::Union { left, right, all } => {
                assert!(left.len() >= 2);
                assert!(right.len() >= 2);
                assert!(!all);
            }
            _ => panic!("expected Union"),
        }
    }

    #[test]
    fn parse_union_all() {
        let stmt = parse_statement(r#"from a | take 3 union all from b | take 3"#).unwrap();
        match stmt {
            CenqlStatement::Union { all, .. } => assert!(all),
            _ => panic!("expected Union"),
        }
    }

    #[test]
    fn parse_intersect() {
        let stmt = parse_statement(r#"from active_users intersect from verified_users"#).unwrap();
        match stmt {
            CenqlStatement::Intersect { .. } => {}
            _ => panic!("expected Intersect"),
        }
    }

    #[test]
    fn parse_except() {
        let stmt = parse_statement(r#"from all_users except from banned_users"#).unwrap();
        match stmt {
            CenqlStatement::Except { .. } => {}
            _ => panic!("expected Except"),
        }
    }

    #[test]
    fn parse_distinct() {
        let stmt = parse_statement(r#"distinct from users | select { country }"#).unwrap();
        match stmt {
            CenqlStatement::Distinct { pipeline } => {
                assert!(pipeline.len() >= 2);
            }
            _ => panic!("expected Distinct"),
        }
    }

    // ========================================================================
    // CTE tests.
    // ========================================================================

    #[test]
    fn parse_cte() {
        let stmt = parse_statement(r#"with active as (from users | filter status == "active") from active | take 10"#).unwrap();
        match stmt {
            CenqlStatement::WithCte { cte_name, cte_pipeline, main_pipeline } => {
                assert_eq!(cte_name, "active");
                assert!(cte_pipeline.len() >= 2);
                assert!(main_pipeline.len() >= 2);
            }
            _ => panic!("expected WithCte"),
        }
    }

    // ========================================================================
    // Query as statement tests.
    // ========================================================================

    #[test]
    fn parse_query_as_statement() {
        let stmt = parse_statement(r#"from users | filter age > 18 | take 10"#).unwrap();
        match stmt {
            CenqlStatement::Query(p) => assert_eq!(p.len(), 3),
            _ => panic!("expected Query"),
        }
    }

    // ========================================================================
    // Error handling tests.
    // ========================================================================

    #[test]
    fn parse_create_table_missing_brace() {
        let result = parse_statement("create table users id: i64");
        assert!(result.is_err());
    }

    #[test]
    fn parse_insert_missing_into() {
        let result = parse_statement(r#"insert users { id: 1 }"#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_drop_missing_keyword() {
        let result = parse_statement("drop users");
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_input() {
        let result = parse_statement("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_create_index_missing_on() {
        let result = parse_statement("create index idx users(name)");
        assert!(result.is_err());
    }

    // ========================================================================
    // Statement display roundtrip tests.
    // ========================================================================

    #[test]
    fn create_table_display_roundtrip() {
        let stmt = parse_statement(r#"create table t { a: i64 not null, b: str }"#).unwrap();
        let displayed = format!("{}", stmt);
        assert!(displayed.contains("create table t"));
        assert!(displayed.contains("a"));
        assert!(displayed.contains("i64"));
    }

    #[test]
    fn insert_display_roundtrip() {
        let stmt = parse_statement(r#"insert into t { a: 1, b: "hello" }"#).unwrap();
        let displayed = format!("{}", stmt);
        assert!(displayed.contains("insert into t"));
    }

    #[test]
    fn begin_commit_display() {
        let begin = parse_statement("begin").unwrap();
        assert_eq!(format!("{}", begin), "begin");
        let commit = parse_statement("commit").unwrap();
        assert_eq!(format!("{}", commit), "commit");
        let rollback = parse_statement("rollback").unwrap();
        assert_eq!(format!("{}", rollback), "rollback");
    }
}
