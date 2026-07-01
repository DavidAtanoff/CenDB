//! CenQL lexer: zero-alloc tokenizer.
//!
//! Tokenises a CenQL source string into a stream of `Token`s. The lexer
//! is hand-written (no `logos` dependency) to keep the binary lean.

use std::fmt;

// ============================================================================
// Token kinds.
// ============================================================================

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    // Keywords.
    From,
    Filter,
    Select,
    Sort,
    Asc,
    Desc,
    Take,
    Join,
    On,
    Inner,
    Left,
    Right,
    Full,
    GroupBy,
    Window,
    Tumbling,
    Hopping,
    Session,
    Match,
    Return,
    Distinct,
    Last,
    And,
    Or,
    Not,

    // Identifiers and literals.
    Ident,
    I64,
    F64,
    Str,
    Duration, // e.g. 5m, 1h, 30s

    // Punctuation.
    Pipe,        // |
    LBrace,      // {
    RBrace,      // }
    LParen,      // (
    RParen,      // )
    LBracket,    // [
    RBracket,    // ]
    Comma,       // ,
    Colon,       // :
    Dot,         // .
    Arrow,       // ->
    BackArrow,   // <-
    Star,        // *
    Question,    // ?

    // Operators.
    EqEq,   // ==
    Assign, // = (single equals, for UPDATE SET)
    NotEq,  // !=
    Lt,     // <
    Le,     // <=
    Gt,     // >
    Ge,     // >=
    Plus,   // +
    Minus,  // -
    Slash,  // /
    StarOp, // * (multiplication — same char as Star but contextual)

    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    /// The source text of the token.
    pub text: String,
    /// Byte offset into the source where this token starts.
    pub offset: usize,
}

impl Token {
    pub fn new(kind: TokenKind, text: String, offset: usize) -> Self {
        Self { kind, text, offset }
    }
}

// ============================================================================
// Tokenizer.
// ============================================================================

pub struct Tokenizer<'a> {
    /// Reserved for future diagnostics (e.g. extracting source snippets
    /// in error messages).
    #[allow(dead_code)]
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    /// Tokenise the entire source.
    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            let token = self.next_token()?;
            let is_eof = token.kind == TokenKind::Eof;
            tokens.push(token);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    fn next_token(&mut self) -> Result<Token, LexError> {
        // Skip whitespace and comments.
        loop {
            if self.pos >= self.bytes.len() {
                return Ok(Token::new(TokenKind::Eof, String::new(), self.pos));
            }
            let b = self.bytes[self.pos];
            if b.is_ascii_whitespace() {
                self.pos += 1;
                continue;
            }
            // Line comment: `-- ...`
            if b == b'-' && self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1] == b'-' {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            break;
        }
        let start = self.pos;
        let b = self.bytes[self.pos];

        // Identifier or keyword.
        if b.is_ascii_alphabetic() || b == b'_' {
            return self.lex_ident(start);
        }
        // Number.
        if b.is_ascii_digit() {
            return self.lex_number(start);
        }
        // String literal.
        if b == b'"' {
            return self.lex_string(start);
        }
        // Duration: digit followed by a unit (e.g. `5m`). We lex it as a
        // number first and then promote to Duration in the parser if a
        // unit follows.
        // Punctuation & operators.
        let (kind, len) = match b {
            b'|' => (TokenKind::Pipe, 1),
            b'{' => (TokenKind::LBrace, 1),
            b'}' => (TokenKind::RBrace, 1),
            b'(' => (TokenKind::LParen, 1),
            b')' => (TokenKind::RParen, 1),
            b'[' => (TokenKind::LBracket, 1),
            b']' => (TokenKind::RBracket, 1),
            b',' => (TokenKind::Comma, 1),
            b':' => (TokenKind::Colon, 1),
            b'.' => (TokenKind::Dot, 1),
            b'*' => (TokenKind::Star, 1),
            b'?' => (TokenKind::Question, 1),
            b'+' => (TokenKind::Plus, 1),
            b'-' => {
                // Could be `->` (arrow), `-` (minus), or `--` (comment, handled above).
                if self.peek_at(1) == Some(b'>') {
                    (TokenKind::Arrow, 2)
                } else {
                    (TokenKind::Minus, 1)
                }
            }
            b'/' => (TokenKind::Slash, 1),
            b'<' => {
                if self.peek_at(1) == Some(b'=') {
                    (TokenKind::Le, 2)
                } else if self.peek_at(1) == Some(b'-') {
                    (TokenKind::BackArrow, 2)
                } else {
                    (TokenKind::Lt, 1)
                }
            }
            b'>' => {
                if self.peek_at(1) == Some(b'=') {
                    (TokenKind::Ge, 2)
                } else {
                    (TokenKind::Gt, 1)
                }
            }
            b'=' => {
                if self.peek_at(1) == Some(b'=') {
                    (TokenKind::EqEq, 2)
                } else {
                    (TokenKind::Assign, 1)
                }
            }
            b'!' => {
                if self.peek_at(1) == Some(b'=') {
                    (TokenKind::NotEq, 2)
                } else {
                    return Err(LexError::new("expected '!=', got '!'", start));
                }
            }
            _ => {
                return Err(LexError::new(
                    format!("unexpected character '{}'", b as char),
                    start,
                ));
            }
        };
        let text = std::str::from_utf8(&self.bytes[start..start + len])
            .unwrap()
            .to_string();
        self.pos += len;
        Ok(Token::new(kind, text, start))
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn lex_ident(&mut self, start: usize) -> Result<Token, LexError> {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap().to_string();
        let kind = match text.as_str() {
            "from" => TokenKind::From,
            "filter" => TokenKind::Filter,
            "select" => TokenKind::Select,
            "sort" => TokenKind::Sort,
            "asc" => TokenKind::Asc,
            "desc" => TokenKind::Desc,
            "take" => TokenKind::Take,
            "join" => TokenKind::Join,
            "on" => TokenKind::On,
            "inner" => TokenKind::Inner,
            "left" => TokenKind::Left,
            "right" => TokenKind::Right,
            "full" => TokenKind::Full,
            "group_by" => TokenKind::GroupBy,
            "window" => TokenKind::Window,
            "tumbling" => TokenKind::Tumbling,
            "hopping" => TokenKind::Hopping,
            "session" => TokenKind::Session,
            "match" => TokenKind::Match,
            "return" => TokenKind::Return,
            "distinct" => TokenKind::Distinct,
            "last" => TokenKind::Last,
            "and" => TokenKind::And,
            "or" => TokenKind::Or,
            "not" => TokenKind::Not,
            _ => TokenKind::Ident,
        };
        Ok(Token::new(kind, text, start))
    }

    fn lex_number(&mut self, start: usize) -> Result<Token, LexError> {
        let mut is_float = false;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_digit() {
                self.pos += 1;
            } else if b == b'.' {
                // Could be a float decimal OR a path separator like
                // `user.address.city`. We treat `digit . digit` as float;
                // `digit . letter` as integer + dot.
                if self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1].is_ascii_digit() {
                    is_float = true;
                    self.pos += 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        // Check for a duration unit suffix: `5m`, `1h`, `30s`, `7d`.
        if let Some(&next) = self.bytes.get(self.pos) {
            if matches!(next, b'm' | b'h' | b's' | b'd') {
                // Only treat as duration if the next-next char is not
                // alphanumeric (so `5min` doesn't parse as `5m` + `in`).
                let next_next = self.bytes.get(self.pos + 1).copied();
                if !next_next.map(|b| b.is_ascii_alphanumeric()).unwrap_or(false) {
                    self.pos += 1;
                    let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap().to_string();
                    return Ok(Token::new(TokenKind::Duration, text, start));
                }
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap().to_string();
        let kind = if is_float { TokenKind::F64 } else { TokenKind::I64 };
        Ok(Token::new(kind, text, start))
    }

    fn lex_string(&mut self, start: usize) -> Result<Token, LexError> {
        self.pos += 1; // skip opening quote
        let content_start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                let text = std::str::from_utf8(&self.bytes[content_start..self.pos])
                    .unwrap()
                    .to_string();
                self.pos += 1; // skip closing quote
                return Ok(Token::new(TokenKind::Str, text, start));
            }
            if b == b'\\' && self.pos + 1 < self.bytes.len() {
                self.pos += 2; // skip escape + next char
                continue;
            }
            self.pos += 1;
        }
        Err(LexError::new("unterminated string literal", start))
    }
}

// ============================================================================
// Lex errors.
// ============================================================================

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub offset: usize,
}

impl LexError {
    pub fn new(msg: impl Into<String>, offset: usize) -> Self {
        Self {
            message: msg.into(),
            offset,
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at offset {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for LexError {}

// ============================================================================
// Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_basic_pipeline() {
        let src = "from users\n| filter age >= 18\n| take 100";
        let tokens = Tokenizer::new(src).tokenize().unwrap();
        assert_eq!(tokens[0].kind, TokenKind::From);
        assert_eq!(tokens[1].kind, TokenKind::Ident);
        assert_eq!(tokens[1].text, "users");
        assert_eq!(tokens[2].kind, TokenKind::Pipe);
        assert_eq!(tokens[3].kind, TokenKind::Filter);
        assert_eq!(tokens[4].text, "age");
        assert_eq!(tokens[5].kind, TokenKind::Ge);
        assert_eq!(tokens[6].kind, TokenKind::I64);
        assert_eq!(tokens[6].text, "18");
        assert_eq!(tokens[7].kind, TokenKind::Pipe);
        assert_eq!(tokens[8].kind, TokenKind::Take);
        assert_eq!(tokens[9].text, "100");
        assert_eq!(tokens[10].kind, TokenKind::Eof);
    }

    #[test]
    fn tokenize_string_literal() {
        let src = "filter name == \"alice\"";
        let tokens = Tokenizer::new(src).tokenize().unwrap();
        // Tokens: Filter(0), Ident(1), EqEq(2), Str(3), Eof(4)
        assert_eq!(tokens[3].kind, TokenKind::Str);
        assert_eq!(tokens[3].text, "alice");
    }

    #[test]
    fn tokenize_duration() {
        let src = "window tumbling(5m) on ts";
        let tokens = Tokenizer::new(src).tokenize().unwrap();
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Duration && t.text == "5m"));
    }

    #[test]
    fn tokenize_float() {
        let src = "filter price > 19.99";
        let tokens = Tokenizer::new(src).tokenize().unwrap();
        assert!(tokens.iter().any(|t| t.kind == TokenKind::F64 && t.text == "19.99"));
    }

    #[test]
    fn tokenize_arrow_and_backarrow() {
        let src = "(a)-[:T]->(b) <-[:U]-";
        let tokens = Tokenizer::new(src).tokenize().unwrap();
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Arrow));
        assert!(tokens.iter().any(|t| t.kind == TokenKind::BackArrow));
    }

    #[test]
    fn tokenize_comments() {
        let src = "from users -- this is a comment\n| take 5";
        let tokens = Tokenizer::new(src).tokenize().unwrap();
        // The comment should be skipped.
        assert!(!tokens.iter().any(|t| t.text.contains("comment")));
    }
}
