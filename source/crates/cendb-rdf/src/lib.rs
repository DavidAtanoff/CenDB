//! cendb-rdf: RDF triple store with SPARQL query support for CenDB.
//!
//! This crate provides:
//! - An in-memory RDF triple store with SPO/POS/OSP indexes for O(1) pattern
//!   matching on bound terms.
//! - N-Triples and Turtle (basic) serialization.
//! - A basic SPARQL 1.1 SELECT parser and evaluator that supports triple
//!   patterns, joins, FILTER comparisons, and LIMIT.
//!
//! All fallible operations return [`CenResult`].

pub mod serialize;
pub mod sparql;

use std::collections::{HashMap, HashSet};

use cendb_core::CenResult;

// ============================================================================
// RDF terms and triples
// ============================================================================

/// An RDF term: the type of value that can appear in any position of a triple.
///
/// `Literal`'s second field is `Option<String>` and follows the convention used
/// throughout this crate:
/// - `None` — a plain literal.
/// - `Some("@xx")` — a language-tagged literal (the value after `@` is the
///   BCP-47 language tag, e.g. `"@en"`).
/// - `Some(uri)` — a typed literal whose datatype is the given IRI
///   (e.g. `"http://www.w3.org/2001/XMLSchema#integer"`).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum RdfTerm {
    /// An IRI reference, e.g. `http://example.org/foo`.
    Uri(String),
    /// A blank node, identified by its label (without the `_:` prefix).
    BlankNode(String),
    /// An RDF literal. See [`RdfTerm`] doc for the encoding of the second
    /// field.
    Literal(String, Option<String>),
}

impl RdfTerm {
    /// Construct a URI term.
    pub fn uri(s: impl Into<String>) -> Self {
        RdfTerm::Uri(s.into())
    }

    /// Construct a blank-node term.
    pub fn blank(s: impl Into<String>) -> Self {
        RdfTerm::BlankNode(s.into())
    }

    /// Construct a plain (untyped, untagged) literal.
    pub fn plain_literal(s: impl Into<String>) -> Self {
        RdfTerm::Literal(s.into(), None)
    }

    /// Construct a typed literal with the given datatype IRI.
    pub fn typed_literal(s: impl Into<String>, datatype: impl Into<String>) -> Self {
        RdfTerm::Literal(s.into(), Some(datatype.into()))
    }

    /// Construct a language-tagged literal.
    pub fn lang_literal(s: impl Into<String>, lang: impl Into<String>) -> Self {
        let mut l = String::from("@");
        l.push_str(&lang.into());
        RdfTerm::Literal(s.into(), Some(l))
    }

    /// Returns the datatype IRI if this is a typed literal.
    pub fn datatype(&self) -> Option<&str> {
        match self {
            RdfTerm::Literal(_, Some(d)) if !d.starts_with('@') => Some(d.as_str()),
            _ => None,
        }
    }

    /// Returns the language tag if this is a language-tagged literal.
    pub fn lang(&self) -> Option<&str> {
        match self {
            RdfTerm::Literal(_, Some(d)) if d.starts_with('@') => Some(&d[1..]),
            _ => None,
        }
    }

    /// Returns the lexical form of a literal, or the IRI / blank label for
    /// other term kinds.
    pub fn lexical_form(&self) -> &str {
        match self {
            RdfTerm::Uri(s) | RdfTerm::BlankNode(s) | RdfTerm::Literal(s, _) => s.as_str(),
        }
    }
}

/// An RDF triple: (subject, predicate, object).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Triple {
    pub subject: RdfTerm,
    pub predicate: RdfTerm,
    pub object: RdfTerm,
}

impl Triple {
    pub fn new(subject: RdfTerm, predicate: RdfTerm, object: RdfTerm) -> Self {
        Self { subject, predicate, object }
    }
}

// ============================================================================
// Triple store
// ============================================================================

/// In-memory RDF triple store with three auxiliary indexes (SPO, POS, OSP) for
/// fast pattern matching.
///
/// The primary store is a `HashSet<Triple>` so duplicate inserts are O(1) and
/// `contains`/`len` are trivial. Each index is keyed by one term (S, P, or O)
/// and maps to the list of triples that have that term in the corresponding
/// position; this lets `match_pattern` pick the smallest candidate set for any
/// (s?, p?, o?) query.
pub struct TripleStore {
    triples: HashSet<Triple>,
    idx_s: HashMap<RdfTerm, Vec<Triple>>,
    idx_p: HashMap<RdfTerm, Vec<Triple>>,
    idx_o: HashMap<RdfTerm, Vec<Triple>>,
}

impl Default for TripleStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TripleStore {
    /// Create an empty triple store.
    pub fn new() -> Self {
        Self {
            triples: HashSet::new(),
            idx_s: HashMap::new(),
            idx_p: HashMap::new(),
            idx_o: HashMap::new(),
        }
    }

    /// Insert a triple. Returns `true` if the triple was newly inserted,
    /// `false` if it already existed.
    pub fn insert(&mut self, triple: Triple) -> CenResult<bool> {
        if !self.triples.insert(triple.clone()) {
            return Ok(false);
        }
        self.idx_s.entry(triple.subject.clone()).or_default().push(triple.clone());
        self.idx_p.entry(triple.predicate.clone()).or_default().push(triple.clone());
        self.idx_o.entry(triple.object.clone()).or_default().push(triple);
        Ok(true)
    }

    /// Remove a triple. Returns `true` if the triple was present and removed.
    pub fn remove(&mut self, triple: &Triple) -> CenResult<bool> {
        if !self.triples.remove(triple) {
            return Ok(false);
        }
        if let Some(v) = self.idx_s.get_mut(&triple.subject) {
            v.retain(|t| t != triple);
        }
        if let Some(v) = self.idx_p.get_mut(&triple.predicate) {
            v.retain(|t| t != triple);
        }
        if let Some(v) = self.idx_o.get_mut(&triple.object) {
            v.retain(|t| t != triple);
        }
        Ok(true)
    }

    /// Returns the number of triples in the store.
    pub fn len(&self) -> usize {
        self.triples.len()
    }

    /// Returns `true` if the store contains no triples.
    pub fn is_empty(&self) -> bool {
        self.triples.is_empty()
    }

    /// Returns `true` if the store contains the given triple.
    pub fn contains(&self, triple: &Triple) -> bool {
        self.triples.contains(triple)
    }

    /// Returns an iterator over all triples in the store.
    pub fn iter(&self) -> impl Iterator<Item = &Triple> {
        self.triples.iter()
    }

    /// Pattern-match triples. Any of `s`/`p`/`o` may be `None` (wildcard).
    ///
    /// Uses the most selective available index for O(1) narrowing on the first
    /// bound term, then filters the candidate set by the remaining bounds.
    pub fn match_pattern(
        &self,
        s: Option<&RdfTerm>,
        p: Option<&RdfTerm>,
        o: Option<&RdfTerm>,
    ) -> Vec<&Triple> {
        // Choose the most selective candidate source. The order (s, then p,
        // then o) corresponds to the three indexes SPO, POS, OSP.
        let (candidates, filter_p, filter_o, filter_s): (Vec<&Triple>, bool, bool, bool) =
            if let Some(st) = s {
                let v = self.idx_s.get(st).map(|x| x.iter().collect()).unwrap_or_default();
                (v, p.is_some(), o.is_some(), false)
            } else if let Some(pt) = p {
                let v = self.idx_p.get(pt).map(|x| x.iter().collect()).unwrap_or_default();
                (v, false, o.is_some(), s.is_some())
            } else if let Some(ot) = o {
                let v = self.idx_o.get(ot).map(|x| x.iter().collect()).unwrap_or_default();
                (v, p.is_some(), false, s.is_some())
            } else {
                (self.triples.iter().collect(), false, false, false)
            };

        candidates
            .into_iter()
            .filter(move |t| {
                if filter_s && Some(&t.subject) != s {
                    return false;
                }
                if filter_p && Some(&t.predicate) != p {
                    return false;
                }
                if filter_o && Some(&t.object) != o {
                    return false;
                }
                true
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> RdfTerm {
        RdfTerm::uri(s)
    }
    fn b(s: &str) -> RdfTerm {
        RdfTerm::blank(s)
    }
    fn l(s: &str) -> RdfTerm {
        RdfTerm::plain_literal(s)
    }

    fn t(s: &str, p: &str, o: &str) -> Triple {
        Triple::new(u(s), u(p), u(o))
    }

    // ----------------------------------------------------------------------
    // Triple store: insertion, pattern matching, removal
    // ----------------------------------------------------------------------

    #[test]
    fn insert_returns_true_for_new_and_false_for_duplicate() {
        let mut store = TripleStore::new();
        assert_eq!(store.len(), 0);
        assert!(store.insert(t("s", "p", "o")).unwrap());
        assert!(!store.insert(t("s", "p", "o")).unwrap());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn match_pattern_spo_full_match() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s1", "p1", "o2")).unwrap();
        store.insert(t("s2", "p1", "o1")).unwrap();
        let r = store.match_pattern(Some(&u("s1")), Some(&u("p1")), Some(&u("o1")));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], &t("s1", "p1", "o1"));
    }

    #[test]
    fn match_pattern_s_wildcard_p_o() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s1", "p2", "o2")).unwrap();
        store.insert(t("s2", "p1", "o1")).unwrap();
        let r = store.match_pattern(Some(&u("s1")), None, None);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn match_pattern_s_p_wildcard_o() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s1", "p1", "o2")).unwrap();
        store.insert(t("s1", "p2", "o3")).unwrap();
        let r = store.match_pattern(Some(&u("s1")), Some(&u("p1")), None);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn match_pattern_wildcard_s_p_o() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s2", "p1", "o2")).unwrap();
        store.insert(t("s3", "p2", "o1")).unwrap();
        let r = store.match_pattern(None, Some(&u("p1")), None);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn match_pattern_s_wildcard_p_o_specific() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s1", "p2", "o1")).unwrap();
        store.insert(t("s2", "p1", "o1")).unwrap();
        let r = store.match_pattern(Some(&u("s1")), None, Some(&u("o1")));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn match_pattern_wildcard_all_returns_everything() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s2", "p2", "o2")).unwrap();
        let r = store.match_pattern(None, None, None);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn match_pattern_wildcard_s_wildcard_p_o() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s2", "p2", "o1")).unwrap();
        store.insert(t("s3", "p3", "o3")).unwrap();
        let r = store.match_pattern(None, None, Some(&u("o1")));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn remove_returns_true_when_present_false_otherwise() {
        let mut store = TripleStore::new();
        store.insert(t("s", "p", "o")).unwrap();
        assert!(store.contains(&t("s", "p", "o")));
        assert!(store.remove(&t("s", "p", "o")).unwrap());
        assert!(!store.contains(&t("s", "p", "o")));
        assert!(!store.remove(&t("s", "p", "o")).unwrap());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn blank_nodes_round_trip_in_store() {
        let mut store = TripleStore::new();
        let triple = Triple::new(b("b1"), u("p"), l("hi"));
        store.insert(triple.clone()).unwrap();
        let r = store.match_pattern(Some(&b("b1")), None, None);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0], &triple);
    }

    #[test]
    fn typed_literals_distinguished_from_plain() {
        let mut store = TripleStore::new();
        store
            .insert(Triple::new(
                u("s"),
                u("p"),
                RdfTerm::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer"),
            ))
            .unwrap();
        store.insert(Triple::new(u("s"), u("p"), l("42"))).unwrap();
        assert_eq!(store.len(), 2);
        let r = store.match_pattern(Some(&u("s")), Some(&u("p")), None);
        assert_eq!(r.len(), 2);
    }

    // ----------------------------------------------------------------------
    // Serialization round-trips
    // ----------------------------------------------------------------------

    #[test]
    fn ntriples_round_trip() {
        let triples = vec![
            Triple::new(u("http://a/s1"), u("http://a/p1"), l("o1")),
            Triple::new(
                u("http://a/s2"),
                u("http://a/p2"),
                RdfTerm::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer"),
            ),
            Triple::new(b("b1"), u("http://a/p3"), RdfTerm::lang_literal("hello", "en")),
        ];
        let s = serialize::serialize_ntriples(&triples);
        let parsed = serialize::parse_ntriples(&s).unwrap();
        assert_eq!(parsed.len(), 3);
        for t in &triples {
            assert!(parsed.contains(t), "missing triple: {:?}", t);
        }
    }

    #[test]
    fn ntriples_parse_basic() {
        let input = "<http://a/s> <http://a/p> <http://a/o> .\n\
                     <http://a/s2> <http://a/p2> \"literal\" .\n";
        let parsed = serialize::parse_ntriples(input).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].subject, u("http://a/s"));
        assert_eq!(parsed[1].object, l("literal"));
    }

    #[test]
    fn turtle_parse_with_prefixes() {
        let input = "@prefix foaf: <http://xmlns.com/foaf/0.1/> .\n\
                     @prefix : <http://example.org/> .\n\
                     :alice foaf:name \"Alice\" ;\n\
                     foaf:knows :bob , :carol .\n";
        let parsed = serialize::parse_turtle(input).unwrap();
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains(&Triple::new(
            u("http://example.org/alice"),
            u("http://xmlns.com/foaf/0.1/name"),
            l("Alice")
        )));
        assert!(parsed.contains(&Triple::new(
            u("http://example.org/alice"),
            u("http://xmlns.com/foaf/0.1/knows"),
            u("http://example.org/bob")
        )));
        assert!(parsed.contains(&Triple::new(
            u("http://example.org/alice"),
            u("http://xmlns.com/foaf/0.1/knows"),
            u("http://example.org/carol")
        )));
    }

    #[test]
    fn turtle_serialize_round_trip() {
        let triples = vec![
            Triple::new(u("http://a/s"), u("http://a/p"), l("o")),
            Triple::new(u("http://a/s"), u("http://a/p2"), u("http://a/o2")),
        ];
        let s = serialize::serialize_turtle(&triples);
        let parsed = serialize::parse_turtle(&s).unwrap();
        assert_eq!(parsed.len(), 2);
        for t in &triples {
            assert!(parsed.contains(t));
        }
    }

    #[test]
    fn turtle_a_keyword_means_rdf_type() {
        let input = "@prefix : <http://example.org/> .\n\
                     :x a :Thing .\n";
        let parsed = serialize::parse_turtle(input).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].predicate,
            u("http://www.w3.org/1999/02/22-rdf-syntax-ns#type")
        );
    }

    // ----------------------------------------------------------------------
    // SPARQL
    // ----------------------------------------------------------------------

    #[test]
    fn sparql_select_single_pattern() {
        let mut store = TripleStore::new();
        store.insert(t("alice", "knows", "bob")).unwrap();
        store.insert(t("alice", "knows", "carol")).unwrap();
        store.insert(t("bob", "knows", "carol")).unwrap();

        let q = sparql::parse_sparql(
            "SELECT ?o WHERE { <alice> <knows> ?o . }",
        )
        .unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        assert_eq!(results.len(), 2);
        let objs: Vec<String> = results
            .iter()
            .map(|b| b.get("o").unwrap().lexical_form().to_string())
            .collect();
        assert!(objs.contains(&"bob".to_string()));
        assert!(objs.contains(&"carol".to_string()));
    }

    #[test]
    fn sparql_select_join_two_patterns() {
        let mut store = TripleStore::new();
        store.insert(t("alice", "knows", "bob")).unwrap();
        store.insert(t("bob", "knows", "carol")).unwrap();
        store.insert(t("alice", "knows", "dave")).unwrap();
        store.insert(t("dave", "knows", "eve")).unwrap();

        let q = sparql::parse_sparql(
            "SELECT ?friend WHERE { <alice> <knows> ?friend . ?friend <knows> ?foaf . }",
        )
        .unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        // Only bob (knows carol) qualifies; dave knows eve too — so two friends
        // of alice (bob, dave) both know someone. The selected variable is
        // ?friend, so we get one row per (friend, foaf) pair → 2 rows.
        assert_eq!(results.len(), 2);
        let friends: Vec<&str> = results
            .iter()
            .map(|b| b.get("friend").unwrap().lexical_form())
            .collect();
        assert!(friends.contains(&"bob"));
        assert!(friends.contains(&"dave"));
    }

    #[test]
    fn sparql_filter_numeric_gt() {
        let mut store = TripleStore::new();
        store
            .insert(Triple::new(
                u("alice"),
                u("age"),
                RdfTerm::typed_literal("30", "http://www.w3.org/2001/XMLSchema#integer"),
            ))
            .unwrap();
        store
            .insert(Triple::new(
                u("bob"),
                u("age"),
                RdfTerm::typed_literal("25", "http://www.w3.org/2001/XMLSchema#integer"),
            ))
            .unwrap();
        store
            .insert(Triple::new(
                u("carol"),
                u("age"),
                RdfTerm::typed_literal("40", "http://www.w3.org/2001/XMLSchema#integer"),
            ))
            .unwrap();

        let q = sparql::parse_sparql(
            "SELECT ?s ?a WHERE { ?s <age> ?a . FILTER(?a > 28) }",
        )
        .unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        // alice (30) and carol (40) qualify, bob (25) does not.
        assert_eq!(results.len(), 2);
        let subjects: Vec<&str> = results.iter().map(|b| b.get("s").unwrap().lexical_form()).collect();
        assert!(subjects.contains(&"alice"));
        assert!(subjects.contains(&"carol"));
    }

    #[test]
    fn sparql_limit_caps_results() {
        let mut store = TripleStore::new();
        for i in 0..10 {
            store.insert(t("s", "p", &format!("o{i}"))).unwrap();
        }
        let q = sparql::parse_sparql("SELECT ?o WHERE { ?s ?p ?o . } LIMIT 3").unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn sparql_select_star_returns_all_variables() {
        let mut store = TripleStore::new();
        store.insert(t("s", "p", "o")).unwrap();
        let q = sparql::parse_sparql("SELECT * WHERE { ?s ?p ?o . }").unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        assert_eq!(results.len(), 1);
        let row = &results[0];
        assert_eq!(row.get("s").unwrap().lexical_form(), "s");
        assert_eq!(row.get("p").unwrap().lexical_form(), "p");
        assert_eq!(row.get("o").unwrap().lexical_form(), "o");
    }

    #[test]
    fn sparql_pattern_with_two_bound_terms() {
        let mut store = TripleStore::new();
        store.insert(t("s1", "p1", "o1")).unwrap();
        store.insert(t("s1", "p1", "o2")).unwrap();
        store.insert(t("s1", "p2", "o3")).unwrap();
        store.insert(t("s2", "p1", "o1")).unwrap();

        let q = sparql::parse_sparql(
            "SELECT ?o WHERE { <s1> <p1> ?o . }",
        )
        .unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn sparql_filter_string_eq() {
        let mut store = TripleStore::new();
        store.insert(Triple::new(u("alice"), u("name"), l("Alice"))).unwrap();
        store.insert(Triple::new(u("bob"), u("name"), l("Bob"))).unwrap();

        let q = sparql::parse_sparql(
            "SELECT ?s WHERE { ?s <name> ?n . FILTER(?n = \"Alice\") }",
        )
        .unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].get("s").unwrap().lexical_form(), "alice");
    }

    #[test]
    fn sparql_blank_node_in_query() {
        let mut store = TripleStore::new();
        store.insert(Triple::new(b("_x"), u("p"), l("v"))).unwrap();
        store.insert(Triple::new(u("s"), u("p"), l("v"))).unwrap();

        let q = sparql::parse_sparql("SELECT ?s WHERE { ?s ?p ?o . }").unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn sparql_parse_rejects_malformed() {
        assert!(sparql::parse_sparql("NOT A QUERY").is_err());
        assert!(sparql::parse_sparql("SELECT WHERE { ?s ?p ?o . }").is_err());
        // WHERE clause with no patterns is rejected.
        assert!(sparql::parse_sparql("SELECT ?s WHERE { }").is_err());
        // SELECT with no variables and no `*`.
        assert!(sparql::parse_sparql("SELECT WHERE { ?s ?p ?o . }").is_err());
    }

    #[test]
    fn sparql_join_with_shared_variable() {
        // Classic join: find pairs (x, y) where x knows y and y knows z.
        let mut store = TripleStore::new();
        store.insert(t("a", "k", "b")).unwrap();
        store.insert(t("b", "k", "c")).unwrap();
        store.insert(t("c", "k", "d")).unwrap();
        // 'a' does not know anyone that knows someone — wait, a→b→c qualifies.

        let q = sparql::parse_sparql(
            "SELECT ?x ?z WHERE { ?x <k> ?y . ?y <k> ?z . }",
        )
        .unwrap();
        let results = sparql::evaluate(&q, &store).unwrap();
        // (a, c) via a→b→c and (b, d) via b→c→d.
        assert_eq!(results.len(), 2);
        let pairs: Vec<(&str, &str)> = results
            .iter()
            .map(|b| {
                (
                    b.get("x").unwrap().lexical_form(),
                    b.get("z").unwrap().lexical_form(),
                )
            })
            .collect();
        assert!(pairs.contains(&("a", "c")));
        assert!(pairs.contains(&("b", "d")));
    }
}
