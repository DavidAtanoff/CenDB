//! RDF serialization: N-Triples and a basic subset of Turtle.
//!
//! ## N-Triples
//! Each non-empty, non-comment line is `<subject> <predicate> <object> .`
//! where subject/predicate are IRIs (or blank nodes for subject) and object is
//! an IRI, blank node, or literal (with optional `@lang` or `^^<datatype>`).
//!
//! ## Turtle (basic subset)
//! Supports `@prefix` declarations, prefixed names (`foaf:name`, `:local`),
//! the `a` keyword (rdf:type), `,` and `;` abbreviations, and the standard
//! literal syntax. Does not support collection syntax `( … )`, blank-node
//! property lists `[ … ]`, or numeric/boolean literal shorthand — those
//! shorthand literals are emitted as plain string literals.
//!
//! Both parsers are deliberately permissive: blank lines and `#` comments are
//! ignored, escape sequences `\"`, `\\`, `\n`, `\t`, `\r` are honoured in
//! literals, and IRIs are stored verbatim (no percent-decoding).

use cendb_core::{CenError, CenResult, CenStatus};

use crate::{RdfTerm, Triple};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Format an [`RdfTerm`] in N-Triples / Turtle syntax.
pub fn format_term(term: &RdfTerm) -> String {
    match term {
        RdfTerm::Uri(s) => format!("<{}>", s),
        RdfTerm::BlankNode(s) => format!("_:{}", s),
        RdfTerm::Literal(value, None) => format!("\"{}\"", escape_literal(value)),
        RdfTerm::Literal(value, Some(tag)) => {
            if let Some(lang) = tag.strip_prefix('@') {
                format!("\"{}\"@{}", escape_literal(value), lang)
            } else {
                format!("\"{}\"^^<{}>", escape_literal(value), tag)
            }
        }
    }
}

fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

// ============================================================================
// N-Triples
// ============================================================================

/// Parse N-Triples input into a list of triples.
pub fn parse_ntriples(input: &str) -> CenResult<Vec<Triple>> {
    let mut out = Vec::new();
    for (lineno, raw) in input.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let triple = parse_ntriples_line(line)
            .map_err(|e| CenError::new(CenStatus::ErrSyntax, format!("line {}: {}", lineno + 1, e)))?;
        out.push(triple);
    }
    Ok(out)
}

fn parse_ntriples_line(line: &str) -> CenResult<Triple> {
    let mut p = Parser::new(line);
    p.skip_ws();
    let subject = p.parse_subject()?;
    p.skip_ws();
    let predicate = p.parse_iri_or_a()?;
    p.skip_ws();
    let object = p.parse_object()?;
    p.skip_ws();
    p.expect_char('.')?;
    Ok(Triple::new(subject, predicate, object))
}

/// Serialize a list of triples as N-Triples (one triple per line).
pub fn serialize_ntriples(triples: &[Triple]) -> String {
    let mut out = String::new();
    for t in triples {
        out.push_str(&format_term(&t.subject));
        out.push(' ');
        out.push_str(&format_term(&t.predicate));
        out.push(' ');
        out.push_str(&format_term(&t.object));
        out.push_str(" .\n");
    }
    out
}

// ============================================================================
// Turtle (basic subset)
// ============================================================================

/// Parse Turtle input into a list of triples.
///
/// Supports: `@prefix` declarations (including the empty-prefix `:`), prefixed
/// names, the `a` keyword, `,` and `;` abbreviations, IRIs, blank nodes, and
/// plain/typed/language-tagged literals.
pub fn parse_turtle(input: &str) -> CenResult<Vec<Triple>> {
    let mut prefixes: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut p = Parser::new(input);

    loop {
        p.skip_ws_and_comments();
        if p.is_eof() {
            break;
        }
        if p.peek_keyword("PREFIX") || p.peek_keyword("@prefix") {
            parse_prefix_decl(&mut p, &mut prefixes, false)?;
            continue;
        }
        if p.peek_keyword("BASE") || p.peek_keyword("@base") {
            // Skip base declaration — we don't resolve relative IRIs, just consume.
            parse_prefix_decl(&mut p, &mut prefixes, true)?;
            continue;
        }
        // Otherwise it's a triples block.
        parse_triples_block(&mut p, &prefixes, &mut out)?;
    }
    Ok(out)
}

fn parse_prefix_decl(
    p: &mut Parser,
    prefixes: &mut HashMap<String, String>,
    is_base: bool,
) -> CenResult<()> {
    // Consume the keyword.
    if p.peek_keyword("@prefix") {
        p.consume_keyword("@prefix");
    } else if p.peek_keyword("PREFIX") {
        p.consume_keyword("PREFIX");
    } else if p.peek_keyword("@base") {
        p.consume_keyword("@base");
    } else if p.peek_keyword("BASE") {
        p.consume_keyword("BASE");
    } else {
        return Err(syntax_err("expected @prefix or PREFIX"));
    }
    p.skip_ws();

    if is_base {
        // Just consume the IRI and optional trailing dot.
        let _iri = p.parse_iri()?;
        p.skip_ws();
        if p.peek_char() == Some('.') {
            p.bump();
        }
        return Ok(());
    }

    // Prefix label — anything up to ':'.
    let mut label = String::new();
    while let Some(c) = p.peek_char() {
        if c == ':' {
            break;
        }
        label.push(c);
        p.bump();
    }
    p.expect_char(':')?;
    p.skip_ws();
    let iri = p.parse_iri()?;
    p.skip_ws();
    // SPARQL-style PREFIX doesn't have a trailing dot; Turtle-style does.
    if p.peek_char() == Some('.') {
        p.bump();
    }
    prefixes.insert(label, iri);
    Ok(())
}

fn parse_triples_block(
    p: &mut Parser,
    prefixes: &HashMap<String, String>,
    out: &mut Vec<Triple>,
) -> CenResult<()> {
    p.skip_ws();
    let subject = parse_term(p, prefixes, true)?;
    p.skip_ws();

    // Parse one or more predicate-object lists separated by ';', terminated by '.'.
    loop {
        p.skip_ws();
        // Allow trailing ';' before '.'.
        if p.peek_char() == Some('.') {
            p.bump();
            break;
        }
        let predicate = parse_predicate(p, prefixes)?;
        p.skip_ws();
        // Parse one or more objects separated by ','.
        loop {
            let object = parse_term(p, prefixes, false)?;
            out.push(Triple::new(subject.clone(), predicate.clone(), object));
            p.skip_ws();
            if p.peek_char() == Some(',') {
                p.bump();
                p.skip_ws();
                continue;
            }
            break;
        }
        p.skip_ws();
        if p.peek_char() == Some(';') {
            p.bump();
            p.skip_ws();
            // Could be followed by '.' (end) — loop handles it.
            continue;
        }
        if p.peek_char() == Some('.') {
            p.bump();
            break;
        }
        return Err(syntax_err("expected '.' or ';' after predicate-object list"));
    }
    Ok(())
}

fn parse_predicate(p: &mut Parser, prefixes: &HashMap<String, String>) -> CenResult<RdfTerm> {
    p.skip_ws();
    if p.peek_char() == Some('a') {
        // Lookahead: 'a' followed by whitespace or end → rdf:type.
        let rest = &p.input[p.pos..];
        if rest.len() == 1 || rest[1..].starts_with(|c: char| c.is_whitespace()) {
            p.bump();
            return Ok(RdfTerm::Uri(RDF_TYPE.to_string()));
        }
    }
    parse_term(p, prefixes, true)
}

fn parse_term(
    p: &mut Parser,
    prefixes: &HashMap<String, String>,
    _allow_iri: bool,
) -> CenResult<RdfTerm> {
    p.skip_ws();
    match p.peek_char() {
        Some('<') => p.parse_iri().map(RdfTerm::Uri),
        Some('_') => {
            // Blank node: _:label
            p.expect_char('_')?;
            p.expect_char(':')?;
            let label = p.parse_pname_local();
            Ok(RdfTerm::BlankNode(label))
        }
        Some('"') => p.parse_literal(),
        Some('\'') => p.parse_literal(),
        Some(c) if c.is_alphabetic() || c == ':' => {
            // Prefixed name (e.g. foaf:name, :local).
            let (prefix, local) = p.parse_prefixed_name()?;
            let base = prefixes
                .get(&prefix)
                .ok_or_else(|| syntax_err(format!("unknown prefix '{}'", prefix)))?;
            Ok(RdfTerm::Uri(format!("{}{}", base, local)))
        }
        Some(c) => Err(syntax_err(format!("unexpected character '{}'", c))),
        None => Err(syntax_err("unexpected end of input")),
    }
}

/// Serialize triples as Turtle. Uses no prefix declarations — every IRI is
/// written in full `<…>` form — but does collapse repeated subjects via `;`
/// and repeated predicate/object pairs via `,` so the output is reasonably
/// compact and round-trips through `parse_turtle`.
pub fn serialize_turtle(triples: &[Triple]) -> String {
    // Group by subject.
    let mut by_subject: VecMap<RdfTerm, (RdfTerm, RdfTerm)> = VecMap::new();
    for t in triples {
        by_subject.push(t.subject.clone(), (t.predicate.clone(), t.object.clone()));
    }
    let mut out = String::new();
    for (subject, pos) in by_subject.into_iter() {
        out.push_str(&format_term(&subject));
        // Group by predicate within the subject.
        let mut by_pred: VecMap<RdfTerm, RdfTerm> = VecMap::new();
        for (p, o) in pos {
            by_pred.push(p, o);
        }
        let mut first_pred = true;
        for (pred, objs) in by_pred.into_iter() {
            if first_pred {
                out.push(' ');
                first_pred = false;
            } else {
                out.push_str(" ;\n  ");
            }
            out.push_str(&format_term(&pred));
            out.push(' ');
            for (i, o) in objs.iter().enumerate() {
                if i > 0 {
                    out.push_str(" , ");
                }
                out.push_str(&format_term(o));
            }
        }
        out.push_str(" .\n");
    }
    out
}

/// Minimal insertion-order-preserving map for serialization grouping. We don't
/// use a HashMap because we want a stable, deterministic output order.
struct VecMap<K, V> {
    keys: Vec<K>,
    values: Vec<Vec<V>>,
}

impl<K: Clone + PartialEq, V> VecMap<K, V> {
    fn new() -> Self {
        Self { keys: Vec::new(), values: Vec::new() }
    }
    fn push(&mut self, k: K, v: V) {
        if let Some(idx) = self.keys.iter().position(|x| x == &k) {
            self.values[idx].push(v);
        } else {
            self.keys.push(k);
            self.values.push(vec![v]);
        }
    }
    fn into_iter(self) -> impl Iterator<Item = (K, Vec<V>)> {
        self.keys.into_iter().zip(self.values.into_iter())
    }
}

// ============================================================================
// Shared tokenizer / parser primitives
// ============================================================================

use std::collections::HashMap;

struct Parser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, bytes: input.as_bytes(), pos: 0 }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek_char(&self) -> Option<char> {
        self.bytes.get(self.pos).map(|b| *b as char)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += 1;
        Some(c)
    }

    fn expect_char(&mut self, c: char) -> CenResult<()> {
        if self.peek_char() == Some(c) {
            self.bump();
            Ok(())
        } else {
            Err(syntax_err(format!("expected '{}', got {:?}", c, self.peek_char())))
        }
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            self.skip_ws();
            if self.peek_char() == Some('#') {
                // Skip to end of line.
                while let Some(c) = self.peek_char() {
                    if c == '\n' {
                        self.bump();
                        break;
                    }
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    fn peek_keyword(&self, kw: &str) -> bool {
        // Case-sensitive Turtle keywords (Turtle is case-sensitive; SPARQL is
        // case-insensitive but we accept either).
        let kb = kw.as_bytes();
        if self.pos + kb.len() > self.bytes.len() {
            return false;
        }
        if &self.bytes[self.pos..self.pos + kb.len()] != kb {
            return false;
        }
        // Must be followed by a non-name character (or whitespace).
        if let Some(&next) = self.bytes.get(self.pos + kb.len()) {
            let n = next as char;
            if n.is_alphanumeric() || n == '_' || n == ':' || n == '-' {
                return false;
            }
        }
        true
    }

    fn consume_keyword(&mut self, kw: &str) {
        self.pos += kw.len();
    }

    /// Parse an IRI in `<…>` form. The IRI is taken verbatim (no escape
    /// processing) — that matches the N-Triples spec for the subset we support.
    fn parse_iri(&mut self) -> CenResult<String> {
        self.expect_char('<')?;
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c == '>' {
                break;
            }
            if c == '\\' {
                // Allow `\uXXXX` and `\UXXXXXXXX` escapes inside IRIs.
                self.bump();
                self.bump();
                continue;
            }
            self.bump();
        }
        let iri = self.input[start..self.pos].to_string();
        self.expect_char('>')?;
        Ok(iri)
    }

    fn parse_subject(&mut self) -> CenResult<RdfTerm> {
        self.skip_ws();
        match self.peek_char() {
            Some('<') => self.parse_iri().map(RdfTerm::Uri),
            Some('_') => {
                self.expect_char('_')?;
                self.expect_char(':')?;
                let label = self.parse_pname_local();
                Ok(RdfTerm::BlankNode(label))
            }
            Some(c) if c.is_alphabetic() || c == ':' => {
                let (prefix, local) = self.parse_prefixed_name()?;
                let _ = prefix; // N-Triples doesn't have prefixes; treat as full IRI.
                Ok(RdfTerm::Uri(local))
            }
            _ => Err(syntax_err("expected subject")),
        }
    }

    fn parse_iri_or_a(&mut self) -> CenResult<RdfTerm> {
        self.skip_ws();
        if self.peek_char() == Some('a') {
            let rest = &self.input[self.pos..];
            if rest.len() == 1 || rest[1..].starts_with(|c: char| c.is_whitespace()) {
                self.bump();
                return Ok(RdfTerm::Uri(RDF_TYPE.to_string()));
            }
        }
        match self.peek_char() {
            Some('<') => self.parse_iri().map(RdfTerm::Uri),
            Some(c) if c.is_alphabetic() || c == ':' => {
                // Prefixed name. In N-Triples this isn't valid, but be lenient.
                let (prefix, local) = self.parse_prefixed_name()?;
                let _ = prefix;
                Ok(RdfTerm::Uri(local))
            }
            _ => Err(syntax_err("expected predicate")),
        }
    }

    fn parse_object(&mut self) -> CenResult<RdfTerm> {
        self.skip_ws();
        match self.peek_char() {
            Some('<') => self.parse_iri().map(RdfTerm::Uri),
            Some('_') => {
                self.expect_char('_')?;
                self.expect_char(':')?;
                let label = self.parse_pname_local();
                Ok(RdfTerm::BlankNode(label))
            }
            Some('"') | Some('\'') => self.parse_literal(),
            Some(c) if c.is_alphabetic() || c == ':' => {
                let (prefix, local) = self.parse_prefixed_name()?;
                let _ = prefix;
                Ok(RdfTerm::Uri(local))
            }
            _ => Err(syntax_err("expected object")),
        }
    }

    fn parse_literal(&mut self) -> CenResult<RdfTerm> {
        let quote = self.bump().ok_or_else(|| syntax_err("expected literal"))?;
        let mut value = String::new();
        loop {
            let c = self.bump().ok_or_else(|| syntax_err("unterminated literal"))?;
            if c == quote {
                break;
            }
            if c == '\\' {
                let next = self.bump().ok_or_else(|| syntax_err("bad escape"))?;
                match next {
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    '\'' => value.push('\''),
                    'u' => {
                        let hex: String =
                            (0..4).filter_map(|_| self.bump()).collect();
                        let cp = u32::from_str_radix(&hex, 16)
                            .map_err(|_| syntax_err("bad \\u escape"))?;
                        if let Some(ch) = char::from_u32(cp) {
                            value.push(ch);
                        }
                    }
                    'U' => {
                        let hex: String =
                            (0..8).filter_map(|_| self.bump()).collect();
                        let cp = u32::from_str_radix(&hex, 16)
                            .map_err(|_| syntax_err("bad \\U escape"))?;
                        if let Some(ch) = char::from_u32(cp) {
                            value.push(ch);
                        }
                    }
                    other => {
                        value.push('\\');
                        value.push(other);
                    }
                }
            } else {
                value.push(c);
            }
        }
        // Optional language tag or datatype.
        let mut tag: Option<String> = None;
        if self.peek_char() == Some('@') {
            self.bump();
            let mut lang = String::from("@");
            while let Some(c) = self.peek_char() {
                if c.is_alphanumeric() || c == '-' {
                    lang.push(c);
                    self.bump();
                } else {
                    break;
                }
            }
            tag = Some(lang);
        } else if self.peek_char() == Some('^') {
            self.bump();
            self.expect_char('^')?;
            let dt = self.parse_iri()?;
            tag = Some(dt);
        }
        Ok(RdfTerm::Literal(value, tag))
    }

    /// Parse a prefixed name `prefix:local`. Returns `(prefix, local)` where
    /// `local` may be empty (e.g. `foaf:`).
    fn parse_prefixed_name(&mut self) -> CenResult<(String, String)> {
        let mut prefix = String::new();
        while let Some(c) = self.peek_char() {
            if c == ':' {
                break;
            }
            if c.is_whitespace() || c == '.' || c == ';' || c == ',' || c == '(' || c == ')' {
                return Err(syntax_err("expected ':' in prefixed name"));
            }
            prefix.push(c);
            self.bump();
        }
        self.expect_char(':')?;
        let local = self.parse_pname_local();
        Ok((prefix, local))
    }

    /// Parse the local part of a prefixed name (after `:`). Stops at whitespace
    /// or any of `.`, `;`, `,`, `)`, `(`.
    fn parse_pname_local(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() || c == '.' || c == ';' || c == ',' || c == ')' || c == '(' {
                break;
            }
            s.push(c);
            self.bump();
        }
        s
    }
}

fn syntax_err(msg: impl Into<String>) -> CenError {
    CenError::new(CenStatus::ErrSyntax, msg)
}
