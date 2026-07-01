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
        // Expect EOF.
        if self.peek_kind() != Some(TokenKind::Eof) {
            return Err(self.unexpected("end of pipeline"));
        }
        Ok(CenqlPipeline::new(stages))
    }

    fn parse_from(&mut self) -> ParseResult<CenqlStage> {
        self.expect(TokenKind::From)?;
        let mut name = self.expect_ident()?;
        // Allow two-word source names like `graph social` (used by the
        // graph model). We consume a second identifier if it follows.
        if self.peek_kind() == Some(TokenKind::Ident) {
            let second = self.advance().text.clone();
            name.push(' ');
            name.push_str(&second);
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
    Parser::new(src)?.parse_pipeline()
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
}
