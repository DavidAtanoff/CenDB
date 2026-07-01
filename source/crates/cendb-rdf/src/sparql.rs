//! Basic SPARQL 1.1 parser and evaluator.
//!
//! Supports the following subset of SPARQL:
//! - `SELECT ?v1 ?v2 … WHERE { … }` or `SELECT * WHERE { … }`
//! - Basic graph patterns: a sequence of triple patterns terminated by `.`
//!   (the trailing `.` is optional before `}`).
//! - Optional `FILTER(?var OP value)` where `OP` is one of
//!   `=`, `!=`, `<`, `>`, `<=`, `>=` and `value` is a numeric literal, a
//!   string literal, or an IRI.
//! - Optional `LIMIT n` clause after the closing `}`.
//!
//! The evaluator joins the patterns (inner join, in the order they appear in
//! the WHERE clause), applies the filters, projects the selected variables, and
//! enforces the LIMIT.

use std::collections::HashMap;

use cendb_core::{CenError, CenResult, CenStatus};

use crate::{RdfTerm, Triple, TripleStore};

/// A term that can appear in a triple pattern: either a concrete RDF term or
/// a query variable (named with `?name` or `$name`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PatternTerm {
    Var(String),
    Term(RdfTerm),
}

/// A triple pattern (subject, predicate, object), each a [`PatternTerm`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TriplePattern {
    pub subject: PatternTerm,
    pub predicate: PatternTerm,
    pub object: PatternTerm,
}

/// A comparison filter: `?var OP value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub var: String,
    pub op: ComparisonOp,
    pub value: FilterValue,
}

/// Comparison operator for a [`Filter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

/// The right-hand side of a [`Filter`]: a numeric or string literal, or an IRI.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterValue {
    Integer(i64),
    Float(f64),
    String(String),
    Iri(String),
}

/// The parsed shape of a SELECT query.
#[derive(Clone, Debug, PartialEq)]
pub struct SparqlQuery {
    /// Selected variables. Empty when `SELECT *` was used (the evaluator then
    /// returns every variable that appears in the patterns).
    pub select: Vec<String>,
    /// `true` if `SELECT *` was used.
    pub select_all: bool,
    pub patterns: Vec<TriplePattern>,
    pub filters: Vec<Filter>,
    pub limit: Option<usize>,
}

// ============================================================================
// Tokenizer
// ============================================================================

#[derive(Clone, Debug, PartialEq)]
enum Token {
    // Punctuation
    LBrace,
    RBrace,
    LParen,
    RParen,
    Dot,
    Semi,
    Comma,
    // Keywords (case-insensitive on input; canonicalised here)
    Select,
    Where,
    Filter,
    Limit,
    // Operators
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    // Terms
    Var(String),
    Iri(String),
    Literal(String, Option<String>), // value, optional lang/datatype
    BlankNode(String),
    Number(String),
    // A keyword-style bareword that isn't recognised (treated as IRI shortcut).
    Bareword(String),
    Star,
}

struct Tokenizer<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, bytes: input.as_bytes(), pos: 0 }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek_byte()?;
        self.pos += 1;
        Some(b)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            while let Some(b) = self.peek_byte() {
                if (b as char).is_whitespace() {
                    self.bump();
                } else {
                    break;
                }
            }
            if self.peek_byte() == Some(b'#') {
                while let Some(b) = self.bump() {
                    if b == b'\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn tokenize(&mut self) -> CenResult<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            self.skip_ws_and_comments();
            let b = match self.peek_byte() {
                Some(b) => b,
                None => break,
            };
            let c = b as char;
            match c {
                '{' => {
                    self.bump();
                    out.push(Token::LBrace);
                }
                '}' => {
                    self.bump();
                    out.push(Token::RBrace);
                }
                '(' => {
                    self.bump();
                    out.push(Token::LParen);
                }
                ')' => {
                    self.bump();
                    out.push(Token::RParen);
                }
                '.' => {
                    self.bump();
                    out.push(Token::Dot);
                }
                ';' => {
                    self.bump();
                    out.push(Token::Semi);
                }
                ',' => {
                    self.bump();
                    out.push(Token::Comma);
                }
                '*' => {
                    self.bump();
                    out.push(Token::Star);
                }
                '?' | '$' => {
                    self.bump();
                    let name = self.read_name();
                    if name.is_empty() {
                        return Err(syntax_err("expected variable name after '?'"));
                    }
                    out.push(Token::Var(name));
                }
                '<' => {
                    // Could be IRI, '<=', or '<' (less-than operator).
                    let next = self.bytes.get(self.pos + 1).copied();
                    match next {
                        Some(b'=') => {
                            self.bump();
                            self.bump();
                            out.push(Token::Le);
                        }
                        Some(n) if is_iri_start(n) => {
                            // IRI.
                            let iri = self.read_iri()?;
                            out.push(Token::Iri(iri));
                        }
                        _ => {
                            // Less-than operator.
                            self.bump();
                            out.push(Token::Lt);
                        }
                    }
                }
                '>' => {
                    if self.bytes.get(self.pos + 1) == Some(&b'=') {
                        self.bump();
                        self.bump();
                        out.push(Token::Ge);
                    } else {
                        self.bump();
                        out.push(Token::Gt);
                    }
                }
                '=' => {
                    self.bump();
                    out.push(Token::Eq);
                }
                '!' => {
                    if self.bytes.get(self.pos + 1) == Some(&b'=') {
                        self.bump();
                        self.bump();
                        out.push(Token::Ne);
                    } else {
                        return Err(syntax_err("expected '!=' after '!'"));
                    }
                }
                '"' | '\'' => {
                    let (value, tag) = self.read_literal()?;
                    out.push(Token::Literal(value, tag));
                }
                '_' if self.bytes.get(self.pos + 1) == Some(&b':') => {
                    self.bump();
                    self.bump();
                    let label = self.read_name();
                    out.push(Token::BlankNode(label));
                }
                c if c.is_ascii_digit() || (c == '-' && self.bytes.get(self.pos + 1).map(|b| (*b as char).is_ascii_digit()).unwrap_or(false)) => {
                    let n = self.read_number();
                    out.push(Token::Number(n));
                }
                c if c.is_alphabetic() || c == ':' => {
                    let word = self.read_name();
                    // Map keywords.
                    let upper = word.to_ascii_uppercase();
                    match upper.as_str() {
                        "SELECT" => out.push(Token::Select),
                        "WHERE" => out.push(Token::Where),
                        "FILTER" => out.push(Token::Filter),
                        "LIMIT" => out.push(Token::Limit),
                        "A" => {
                            // `a` keyword = rdf:type, but only when not part of
                            // a longer name. read_name already stops at
                            // non-name chars so this is fine.
                            out.push(Token::Iri(
                                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"
                                    .to_string(),
                            ));
                        }
                        _ => {
                            // If it contains a colon, treat as IRI (prefixed
                            // names in SPARQL — we just store them verbatim
                            // because the parser doesn't know prefixes).
                            if word.contains(':') {
                                out.push(Token::Bareword(word));
                            } else {
                                out.push(Token::Bareword(word));
                            }
                        }
                    }
                }
                _ => {
                    return Err(syntax_err(format!("unexpected character '{}'", c)));
                }
            }
        }
        Ok(out)
    }

    fn read_name(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            let c = b as char;
            if c.is_alphanumeric() || c == '_' || c == '-' || c == ':' || c == '.' {
                // Don't consume a trailing '.' that acts as a statement
                // terminator — but only if it's followed by whitespace/'}'/EOF.
                if c == '.' {
                    let next = self.bytes.get(self.pos + 1).copied();
                    let is_terminator = match next {
                        None => true,
                        Some(n) => (n as char).is_whitespace() || n == b'}',
                    };
                    if is_terminator {
                        break;
                    }
                }
                self.bump();
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_string()
    }

    fn read_iri(&mut self) -> CenResult<String> {
        // Already peeked '<'.
        self.bump(); // consume '<'
        let start = self.pos;
        while let Some(b) = self.peek_byte() {
            if b == b'>' {
                break;
            }
            self.bump();
        }
        let iri = self.input[start..self.pos].to_string();
        if self.bump() != Some(b'>') {
            return Err(syntax_err("unterminated IRI"));
        }
        Ok(iri)
    }

    fn read_literal(&mut self) -> CenResult<(String, Option<String>)> {
        let quote = self.bump().unwrap();
        let mut value = String::new();
        loop {
            let b = match self.bump() {
                Some(b) => b,
                None => return Err(syntax_err("unterminated literal")),
            };
            if b == quote {
                break;
            }
            if b == b'\\' {
                let next = self.bump().ok_or_else(|| syntax_err("bad escape"))?;
                match next {
                    b'"' => value.push('"'),
                    b'\'' => value.push('\''),
                    b'\\' => value.push('\\'),
                    b'n' => value.push('\n'),
                    b'r' => value.push('\r'),
                    b't' => value.push('\t'),
                    b'u' => {
                        let hex: String = (0..4).filter_map(|_| self.bump()).map(|b| b as char).collect();
                        if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                            if let Some(ch) = char::from_u32(cp) {
                                value.push(ch);
                            }
                        }
                    }
                    _ => {
                        value.push('\\');
                        value.push(next as char);
                    }
                }
            } else {
                value.push(b as char);
            }
        }
        // Optional @lang or ^^<datatype>.
        let mut tag: Option<String> = None;
        if self.peek_byte() == Some(b'@') {
            self.bump();
            let mut lang = String::from("@");
            while let Some(b) = self.peek_byte() {
                let c = b as char;
                if c.is_alphanumeric() || c == '-' {
                    lang.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
            tag = Some(lang);
        } else if self.peek_byte() == Some(b'^')
            && self.bytes.get(self.pos + 1) == Some(&b'^')
        {
            self.bump();
            self.bump();
            let dt = self.read_iri()?;
            tag = Some(dt);
        }
        Ok((value, tag))
    }

    fn read_number(&mut self) -> String {
        let start = self.pos;
        let mut seen_dot = false;
        let mut seen_e = false;
        // Handle leading '-'.
        if self.peek_byte() == Some(b'-') {
            self.bump();
        }
        while let Some(b) = self.peek_byte() {
            let c = b as char;
            if c.is_ascii_digit() {
                self.bump();
            } else if c == '.' && !seen_dot && !seen_e {
                seen_dot = true;
                // Don't consume if followed by non-digit (could be statement terminator).
                let next = self.bytes.get(self.pos + 1).copied();
                if next.map(|n| (n as char).is_ascii_digit()).unwrap_or(false) {
                    self.bump();
                } else {
                    break;
                }
            } else if (c == 'e' || c == 'E') && !seen_e {
                seen_e = true;
                self.bump();
                if self.peek_byte() == Some(b'+') || self.peek_byte() == Some(b'-') {
                    self.bump();
                }
            } else {
                break;
            }
        }
        self.input[start..self.pos].to_string()
    }
}

// ============================================================================
// Parser
// ============================================================================

struct TokenStream {
    tokens: Vec<Token>,
    pos: usize,
}

impl TokenStream {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }
    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn expect(&mut self, expected: &Token) -> CenResult<()> {
        match self.bump() {
            Some(t) if &t == expected => Ok(()),
            Some(t) => Err(syntax_err(format!("expected {:?}, got {:?}", expected, t))),
            None => Err(syntax_err(format!("expected {:?}, got EOF", expected))),
        }
    }
}

/// Parse a SPARQL SELECT query.
pub fn parse_sparql(input: &str) -> CenResult<SparqlQuery> {
    let tokens = Tokenizer::new(input).tokenize()?;
    let mut ts = TokenStream::new(tokens);

    // SELECT
    match ts.bump() {
        Some(Token::Select) => {}
        Some(t) => return Err(syntax_err(format!("expected SELECT, got {:?}", t))),
        None => return Err(syntax_err("empty query")),
    }

    // Variable list or '*'.
    let mut select = Vec::new();
    let mut select_all = false;
    match ts.peek() {
        Some(Token::Star) => {
            ts.bump();
            select_all = true;
        }
        _ => {
            while let Some(Token::Var(name)) = ts.peek() {
                select.push(name.clone());
                ts.bump();
            }
            if select.is_empty() {
                return Err(syntax_err("SELECT must be followed by variables or '*'"));
            }
        }
    }

    // Optional WHERE keyword.
    if let Some(Token::Where) = ts.peek() {
        ts.bump();
    }

    // '{' patterns ('.'?)* filters* '}'
    ts.expect(&Token::LBrace)?;

    let mut patterns = Vec::new();
    let mut filters = Vec::new();

    loop {
        match ts.peek() {
            None => return Err(syntax_err("unterminated WHERE clause")),
            Some(Token::RBrace) => {
                ts.bump();
                break;
            }
            Some(Token::Filter) => {
                ts.bump();
                let f = parse_filter(&mut ts)?;
                filters.push(f);
                // Optional trailing dot.
                if let Some(Token::Dot) = ts.peek() {
                    ts.bump();
                }
            }
            Some(Token::Dot) => {
                ts.bump();
            }
            _ => {
                let pat = parse_triple_pattern(&mut ts)?;
                patterns.push(pat);
                // Optional trailing dot.
                if let Some(Token::Dot) = ts.peek() {
                    ts.bump();
                }
            }
        }
    }

    if patterns.is_empty() {
        return Err(syntax_err("WHERE clause must contain at least one triple pattern"));
    }

    // Optional LIMIT n.
    let mut limit = None;
    while let Some(t) = ts.peek() {
        match t {
            Token::Limit => {
                ts.bump();
                match ts.bump() {
                    Some(Token::Number(n)) => {
                        limit = Some(
                            n.parse::<usize>()
                                .map_err(|_| syntax_err(format!("bad LIMIT: {}", n)))?,
                        );
                    }
                    Some(t) => return Err(syntax_err(format!("expected number after LIMIT, got {:?}", t))),
                    None => return Err(syntax_err("expected number after LIMIT")),
                }
            }
            _ => return Err(syntax_err(format!("unexpected token after WHERE clause: {:?}", t))),
        }
    }

    Ok(SparqlQuery { select, select_all, patterns, filters, limit })
}

fn parse_triple_pattern(ts: &mut TokenStream) -> CenResult<TriplePattern> {
    let subject = parse_pattern_term(ts)?;
    let predicate = parse_pattern_term(ts)?;
    let object = parse_pattern_term(ts)?;
    Ok(TriplePattern { subject, predicate, object })
}

fn parse_pattern_term(ts: &mut TokenStream) -> CenResult<PatternTerm> {
    match ts.bump() {
        Some(Token::Var(name)) => Ok(PatternTerm::Var(name)),
        Some(Token::Iri(s)) => Ok(PatternTerm::Term(RdfTerm::Uri(s))),
        Some(Token::Literal(v, tag)) => Ok(PatternTerm::Term(RdfTerm::Literal(v, tag))),
        Some(Token::BlankNode(label)) => Ok(PatternTerm::Term(RdfTerm::BlankNode(label))),
        Some(Token::Bareword(w)) => {
            // Barewords that aren't keywords are treated as prefixed-name IRIs
            // (e.g. `foaf:name` or `knows`). Since we don't track prefixes at
            // the SPARQL layer, store them verbatim as IRIs.
            Ok(PatternTerm::Term(RdfTerm::Uri(w)))
        }
        Some(t) => Err(syntax_err(format!("expected pattern term, got {:?}", t))),
        None => Err(syntax_err("expected pattern term, got EOF")),
    }
}

fn parse_filter(ts: &mut TokenStream) -> CenResult<Filter> {
    ts.expect(&Token::LParen)?;
    let var = match ts.bump() {
        Some(Token::Var(name)) => name,
        Some(t) => return Err(syntax_err(format!("expected variable in FILTER, got {:?}", t))),
        None => return Err(syntax_err("expected variable in FILTER")),
    };
    let op = match ts.bump() {
        Some(Token::Eq) => ComparisonOp::Eq,
        Some(Token::Ne) => ComparisonOp::Ne,
        Some(Token::Lt) => ComparisonOp::Lt,
        Some(Token::Gt) => ComparisonOp::Gt,
        Some(Token::Le) => ComparisonOp::Le,
        Some(Token::Ge) => ComparisonOp::Ge,
        Some(t) => return Err(syntax_err(format!("expected comparison operator, got {:?}", t))),
        None => return Err(syntax_err("expected comparison operator")),
    };
    let value = match ts.bump() {
        Some(Token::Number(n)) => {
            if n.contains('.') || n.contains('e') || n.contains('E') {
                FilterValue::Float(
                    n.parse::<f64>()
                        .map_err(|_| syntax_err(format!("bad number: {}", n)))?,
                )
            } else {
                FilterValue::Integer(
                    n.parse::<i64>()
                        .map_err(|_| syntax_err(format!("bad integer: {}", n)))?,
                )
            }
        }
        Some(Token::Literal(s, _)) => FilterValue::String(s),
        Some(Token::Iri(s)) => FilterValue::Iri(s),
        Some(t) => return Err(syntax_err(format!("expected filter value, got {:?}", t))),
        None => return Err(syntax_err("expected filter value")),
    };
    ts.expect(&Token::RParen)?;
    Ok(Filter { var, op, value })
}

// ============================================================================
// Evaluator
// ============================================================================

/// A binding row: maps variable name → bound RDF term.
pub type Binding = HashMap<String, RdfTerm>;

/// Evaluate a parsed [`SparqlQuery`] against a [`TripleStore`], returning the
/// matching binding rows.
pub fn evaluate(query: &SparqlQuery, store: &TripleStore) -> CenResult<Vec<Binding>> {
    if query.patterns.is_empty() {
        return Ok(Vec::new());
    }

    // Start with all bindings from the first pattern.
    let mut bindings: Vec<Binding> = evaluate_pattern(&query.patterns[0], store);

    // Join each subsequent pattern.
    for pat in &query.patterns[1..] {
        let mut next = Vec::with_capacity(bindings.len());
        for b in &bindings {
            let matches = evaluate_pattern_with_binding(pat, store, b);
            for m in matches {
                let mut merged = b.clone();
                for (k, v) in m {
                    merged.insert(k, v);
                }
                next.push(merged);
            }
        }
        bindings = next;
        if bindings.is_empty() {
            break;
        }
    }

    // Apply filters.
    if !query.filters.is_empty() {
        bindings.retain(|b| query.filters.iter().all(|f| eval_filter(f, b)));
    }

    // Determine the projected variables.
    let project_vars: Vec<String> = if query.select_all {
        // Collect every variable that appears in the patterns, in order.
        let mut seen: Vec<String> = Vec::new();
        for p in &query.patterns {
            for term in [&p.subject, &p.predicate, &p.object] {
                if let PatternTerm::Var(name) = term {
                    if !seen.contains(name) {
                        seen.push(name.clone());
                    }
                }
            }
        }
        seen
    } else {
        query.select.clone()
    };

    // Project: keep only selected variables in each binding.
    if !project_vars.is_empty() {
        for b in &mut bindings {
            b.retain(|k, _| project_vars.iter().any(|v| v == k));
        }
    }

    // Apply LIMIT.
    if let Some(n) = query.limit {
        bindings.truncate(n);
    }

    Ok(bindings)
}

/// Evaluate a single pattern against the store with no prior bindings.
fn evaluate_pattern(pat: &TriplePattern, store: &TripleStore) -> Vec<Binding> {
    let s = pattern_term_to_lookup(&pat.subject, &HashMap::new());
    let p = pattern_term_to_lookup(&pat.predicate, &HashMap::new());
    let o = pattern_term_to_lookup(&pat.object, &HashMap::new());
    let matches = store.match_pattern(s.as_ref(), p.as_ref(), o.as_ref());
    matches
        .into_iter()
        .filter_map(|t| bind_triple(pat, t))
        .collect()
}

/// Evaluate a single pattern with a prior binding row. Only matches that are
/// consistent with the existing binding are returned (the new bindings from
/// the pattern are merged in by the caller).
fn evaluate_pattern_with_binding(
    pat: &TriplePattern,
    store: &TripleStore,
    binding: &Binding,
) -> Vec<Binding> {
    let s = pattern_term_to_lookup(&pat.subject, binding);
    let p = pattern_term_to_lookup(&pat.predicate, binding);
    let o = pattern_term_to_lookup(&pat.object, binding);
    let matches = store.match_pattern(s.as_ref(), p.as_ref(), o.as_ref());
    matches
        .into_iter()
        .filter_map(|t| {
            // Verify shared variables are consistent.
            let mut new_binding: Binding = HashMap::new();
            for (pt, term) in [
                (&pat.subject, &t.subject),
                (&pat.predicate, &t.predicate),
                (&pat.object, &t.object),
            ] {
                match pt {
                    PatternTerm::Var(name) => {
                        if let Some(existing) = binding.get(name) {
                            if existing != term {
                                return None;
                            }
                        } else if let Some(existing) = new_binding.get(name) {
                            if existing != term {
                                return None;
                            }
                        } else {
                            new_binding.insert(name.clone(), term.clone());
                        }
                    }
                    PatternTerm::Term(_) => {}
                }
            }
            Some(new_binding)
        })
        .collect()
}

/// Resolve a pattern term to a concrete term for index lookup, or `None` if
/// it's an unbound variable.
fn pattern_term_to_lookup(term: &PatternTerm, binding: &Binding) -> Option<RdfTerm> {
    match term {
        PatternTerm::Var(name) => binding.get(name).cloned(),
        PatternTerm::Term(t) => Some(t.clone()),
    }
}

/// Try to bind a triple pattern to a concrete triple, producing a binding row.
/// Returns `None` if the pattern has conflicting variable assignments (should
/// not happen for a single triple, but we check defensively).
fn bind_triple(pat: &TriplePattern, t: &Triple) -> Option<Binding> {
    let mut b = Binding::new();
    for (pt, term) in [
        (&pat.subject, &t.subject),
        (&pat.predicate, &t.predicate),
        (&pat.object, &t.object),
    ] {
        if let PatternTerm::Var(name) = pt {
            if let Some(existing) = b.get(name) {
                if existing != term {
                    return None;
                }
            } else {
                b.insert(name.clone(), term.clone());
            }
        }
    }
    Some(b)
}

fn eval_filter(f: &Filter, b: &Binding) -> bool {
    let term = match b.get(&f.var) {
        Some(t) => t,
        None => return false,
    };
    match &f.value {
        FilterValue::Integer(n) => {
            let parsed = parse_literal_as_number(term);
            match (parsed, f.op) {
                (Some(NumVal::Int(x)), ComparisonOp::Eq) => x == *n,
                (Some(NumVal::Int(x)), ComparisonOp::Ne) => x != *n,
                (Some(NumVal::Int(x)), ComparisonOp::Lt) => x < *n,
                (Some(NumVal::Int(x)), ComparisonOp::Gt) => x > *n,
                (Some(NumVal::Int(x)), ComparisonOp::Le) => x <= *n,
                (Some(NumVal::Int(x)), ComparisonOp::Ge) => x >= *n,
                (Some(NumVal::Float(x)), op) => compare_float(x, *n as f64, op),
                (None, _) => false,
            }
        }
        FilterValue::Float(n) => {
            let parsed = parse_literal_as_number(term);
            match parsed {
                Some(NumVal::Int(x)) => compare_float(x as f64, *n, f.op),
                Some(NumVal::Float(x)) => compare_float(x, *n, f.op),
                None => false,
            }
        }
        FilterValue::String(s) => {
            let lex = term.lexical_form();
            match f.op {
                ComparisonOp::Eq => lex == s,
                ComparisonOp::Ne => lex != s,
                ComparisonOp::Lt => lex < s.as_str(),
                ComparisonOp::Gt => lex > s.as_str(),
                ComparisonOp::Le => lex <= s.as_str(),
                ComparisonOp::Ge => lex >= s.as_str(),
            }
        }
        FilterValue::Iri(iri) => {
            if let RdfTerm::Uri(u) = term {
                match f.op {
                    ComparisonOp::Eq => u == iri,
                    ComparisonOp::Ne => u != iri,
                    _ => false,
                }
            } else {
                false
            }
        }
    }
}

enum NumVal {
    Int(i64),
    Float(f64),
}

fn parse_literal_as_number(t: &RdfTerm) -> Option<NumVal> {
    match t {
        RdfTerm::Literal(s, _) => {
            if let Ok(i) = s.parse::<i64>() {
                return Some(NumVal::Int(i));
            }
            if let Ok(f) = s.parse::<f64>() {
                return Some(NumVal::Float(f));
            }
            None
        }
        _ => None,
    }
}

fn compare_float(a: f64, b: f64, op: ComparisonOp) -> bool {
    match op {
        ComparisonOp::Eq => a == b,
        ComparisonOp::Ne => a != b,
        ComparisonOp::Lt => a < b,
        ComparisonOp::Gt => a > b,
        ComparisonOp::Le => a <= b,
        ComparisonOp::Ge => a >= b,
    }
}

fn syntax_err(msg: impl Into<String>) -> CenError {
    CenError::new(CenStatus::ErrSyntax, msg)
}

/// Returns true if the byte could plausibly be the start of an IRI body (i.e.
/// the byte after `<`). Used to disambiguate `<…>` IRIs from `<` (less-than).
fn is_iri_start(b: u8) -> bool {
    let c = b as char;
    c.is_alphanumeric() || c == '/' || c == ':' || c == '#' || c == '_' || c == '-' || c == '~' || c == '%'
}
