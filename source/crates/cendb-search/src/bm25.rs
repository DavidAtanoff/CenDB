//! BM25 (Best Matching 25) relevance ranking.
//!
//! BM25 is the standard family of ranking functions used by search engines to
//! estimate the relevance of documents to a given search query. It is a
//! TF-IDF-like function that additionally applies term-frequency saturation
//! and document-length normalization.
//!
//! ## Formula
//!
//! ```text
//! score(D, Q) = Σ_{t ∈ Q} IDF(t) · (f(t,D) · (k1 + 1))
//!                                 / (f(t,D) + k1 · (1 − b + b · |D| / avgdl))
//! ```
//!
//! where
//!   * `f(t,D)` — term frequency of `t` in document `D`
//!   * `|D|` — length of document `D` (in indexed terms)
//!   * `avgdl` — average document length across the corpus
//!   * `k1 = 1.2` — term-frequency saturation parameter
//!   * `b = 0.75` — length-normalization parameter
//!   * `IDF(t) = ln((N − n(t) + 0.5) / (n(t) + 0.5) + 1)`
//!     where `N` is the total document count and `n(t)` is the number of
//!     documents containing `t`.

use std::collections::HashMap;

use crate::stemmer::stem;
use crate::tokenizer::is_stopword;

/// BM25 scorer. Owns the corpus statistics needed to score documents against
/// a query: total document count, average document length, per-term document
/// frequency, per-document term frequencies, and per-document lengths.
pub struct Bm25Scorer {
    /// Term-frequency saturation parameter (defaults to 1.2).
    k1: f64,
    /// Length-normalization parameter (defaults to 0.75).
    b: f64,
    /// Total number of documents in the corpus (N).
    doc_count: u64,
    /// Average document length (avgdl).
    avg_doc_len: f64,
    /// Document frequency per term: `n(t)` — number of docs containing `t`.
    doc_freq: HashMap<String, u64>,
    /// Document length per doc id: `|D|` (number of indexed terms).
    doc_len: HashMap<u64, usize>,
    /// Term frequencies: `doc_id → (term → tf)`.
    term_freqs: HashMap<u64, HashMap<String, u32>>,
}

impl Bm25Scorer {
    /// Create a scorer with explicit `k1` and `b` parameters and no
    /// documents yet.
    pub fn with_params(k1: f64, b: f64) -> Self {
        Self {
            k1,
            b,
            doc_count: 0,
            avg_doc_len: 0.0,
            doc_freq: HashMap::new(),
            doc_len: HashMap::new(),
            term_freqs: HashMap::new(),
        }
    }

    /// Create a scorer with the standard BM25 parameters (`k1 = 1.2`,
    /// `b = 0.75`).
    pub fn new() -> Self {
        Self::with_params(1.2, 0.75)
    }

    /// Returns the configured `k1` parameter.
    pub fn k1(&self) -> f64 {
        self.k1
    }

    /// Returns the configured `b` parameter.
    pub fn b(&self) -> f64 {
        self.b
    }

    /// Number of documents in the corpus.
    pub fn doc_count(&self) -> u64 {
        self.doc_count
    }

    /// Average document length.
    pub fn avg_doc_len(&self) -> f64 {
        self.avg_doc_len
    }

    /// Document frequency for a term: number of documents containing `term`.
    pub fn doc_freq(&self, term: &str) -> u64 {
        self.doc_freq.get(term).copied().unwrap_or(0)
    }

    /// Document length (number of indexed terms).
    pub fn doc_len(&self, doc_id: u64) -> usize {
        self.doc_len.get(&doc_id).copied().unwrap_or(0)
    }

    /// Term frequency of `term` in `doc_id`.
    pub fn term_frequency(&self, term: &str, doc_id: u64) -> u32 {
        self.term_freqs
            .get(&doc_id)
            .and_then(|m| m.get(term))
            .copied()
            .unwrap_or(0)
    }

    /// Add a document to the corpus. The terms should be pre-stemmed and
    /// lowercased. Document length is taken to be the number of terms
    /// supplied (including repeats). Average document length is refreshed
    /// automatically.
    pub fn add_document(&mut self, doc_id: u64, terms: &[&str]) {
        let mut tf: HashMap<String, u32> = HashMap::new();
        for term in terms {
            *tf.entry((*term).to_string()).or_insert(0) += 1;
        }
        let doc_length = terms.len();
        for term in tf.keys() {
            *self.doc_freq.entry(term.clone()).or_insert(0) += 1;
        }
        self.doc_len.insert(doc_id, doc_length);
        self.term_freqs.insert(doc_id, tf);
        self.doc_count += 1;
        self.refresh_avg();
    }

    /// Recompute and store the average document length. Called automatically
    /// after each `add_document`; exposed for callers that mutate internal
    /// state via other paths.
    fn refresh_avg(&mut self) {
        let total: usize = self.doc_len.values().sum();
        self.avg_doc_len = if self.doc_count == 0 {
            0.0
        } else {
            total as f64 / self.doc_count as f64
        };
    }

    /// Inverse document frequency for `term` using the BM25 formula:
    /// `ln((N − n(t) + 0.5) / (n(t) + 0.5) + 1)`.
    ///
    /// The `+ 1` inside the logarithm keeps IDF non-negative even when a
    /// term appears in every document, which is the form used by Lucene and
    /// most modern BM25 implementations.
    pub fn idf(&self, term: &str) -> f64 {
        if self.doc_count == 0 {
            return 0.0;
        }
        let n_t = self.doc_freq(term);
        let n = self.doc_count as f64;
        let numerator = n - n_t as f64 + 0.5;
        let denominator = n_t as f64 + 0.5;
        (numerator / denominator + 1.0).ln()
    }

    /// Compute the BM25 score for a single document against a set of query
    /// terms (already stemmed). Returns `0.0` if the document is unknown or
    /// no query terms match.
    pub fn score(&self, query_terms: &[&str], doc_id: u64) -> f64 {
        let doc_len = match self.doc_len.get(&doc_id) {
            Some(&l) => l,
            None => return 0.0,
        };
        if self.doc_count == 0 || self.avg_doc_len == 0.0 {
            return 0.0;
        }
        let tf_map = match self.term_freqs.get(&doc_id) {
            Some(m) => m,
            None => return 0.0,
        };

        let mut total = 0.0;
        let unique_terms: std::collections::HashSet<&str> = query_terms.iter().copied().collect();
        for term in unique_terms {
            let idf = self.idf(term);
            if idf <= 0.0 {
                continue;
            }
            let f = tf_map.get(term).copied().unwrap_or(0) as f64;
            if f == 0.0 {
                continue;
            }
            let denom = f + self.k1 * (1.0 - self.b + self.b * (doc_len as f64) / self.avg_doc_len);
            if denom == 0.0 {
                continue;
            }
            total += idf * (f * (self.k1 + 1.0)) / denom;
        }
        total
    }

    /// Rank a list of candidate document IDs by BM25 score against `query`.
    /// The query is tokenized (split on non-alphanumeric, lowercased,
    /// stop-word filtered, and Porter-stemmed) before scoring. Results are
    /// returned in descending score order; documents with score `0.0` are
    /// excluded.
    pub fn rank(&self, query: &str, doc_ids: &[u64]) -> Vec<(u64, f64)> {
        let terms = tokenize_query(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let term_refs: Vec<&str> = terms.iter().map(|s| s.as_str()).collect();
        let mut scored: Vec<(u64, f64)> = doc_ids
            .iter()
            .map(|&id| (id, self.score(&term_refs, id)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored
    }

    /// Build a `Bm25Scorer` from the document texts in `docs`. Each document
    /// is tokenized, stemmed, and indexed. This is the easiest way to
    /// construct a scorer for testing or for standalone use.
    pub fn from_docs(docs: &[(u64, &str)]) -> Self {
        let mut s = Self::new();
        for &(doc_id, text) in docs {
            let terms = tokenize_query(text);
            let term_refs: Vec<&str> = terms.iter().map(|s| s.as_str()).collect();
            s.add_document(doc_id, &term_refs);
        }
        s
    }
}

impl Default for Bm25Scorer {
    fn default() -> Self {
        Self::new()
    }
}

/// Tokenize a query string for BM25 scoring: split on non-alphanumeric
/// characters, lowercase, drop stop-words, and apply the Porter stemmer.
fn tokenize_query(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_lowercase();
        if is_stopword(&lower) {
            continue;
        }
        out.push(stem(&lower));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn default_params() {
        let s = Bm25Scorer::new();
        assert!(approx(s.k1(), 1.2));
        assert!(approx(s.b(), 0.75));
        assert_eq!(s.doc_count(), 0);
    }

    #[test]
    fn add_document_updates_stats() {
        let mut s = Bm25Scorer::new();
        s.add_document(1, &["rust", "database", "rust"]);
        s.refresh_avg();
        assert_eq!(s.doc_count(), 1);
        assert_eq!(s.doc_len(1), 3);
        assert_eq!(s.doc_freq("rust"), 1);
        assert_eq!(s.doc_freq("database"), 1);
        assert_eq!(s.term_frequency("rust", 1), 2);
        assert_eq!(s.term_frequency("database", 1), 1);
        assert!(approx(s.avg_doc_len(), 3.0));
    }

    #[test]
    fn idf_decreases_with_more_docs_containing_term() {
        let mut s = Bm25Scorer::new();
        s.add_document(1, &["rare", "common"]);
        s.add_document(2, &["common"]);
        s.add_document(3, &["common"]);
        s.refresh_avg();
        let idf_rare = s.idf("rare");
        let idf_common = s.idf("common");
        assert!(idf_rare > idf_common, "rare term should have higher IDF");
        // "common" appears in all 3 docs — IDF should still be non-negative
        // due to the +1 inside the log.
        assert!(idf_common >= 0.0);
    }

    #[test]
    fn idf_unknown_term_is_high() {
        // Unknown terms (n_t = 0) should have the highest IDF — they appear
        // in no documents.
        let s = Bm25Scorer::from_docs(&[(1, "hello world"), (2, "foo bar")]);
        let idf_unknown = s.idf("nonexistent");
        let idf_known = s.idf("hello");
        assert!(idf_unknown > idf_known);
        assert!(idf_unknown > 0.0);
    }

    #[test]
    fn score_zero_for_unknown_doc() {
        let s = Bm25Scorer::from_docs(&[(1, "hello world")]);
        assert!(approx(s.score(&["hello"], 999), 0.0));
    }

    #[test]
    fn score_zero_for_empty_corpus() {
        let s = Bm25Scorer::new();
        assert!(approx(s.score(&["anything"], 1), 0.0));
    }

    #[test]
    fn score_zero_for_non_matching_query() {
        let s = Bm25Scorer::from_docs(&[(1, "rust database"), (2, "rust engine")]);
        assert!(approx(s.score(&["nonexistent"], 1), 0.0));
    }

    #[test]
    fn score_known_value_single_doc_single_term() {
        // Single doc with one occurrence of "rust". The IDF term:
        //   idf = ln((1 - 1 + 0.5) / (1 + 0.5) + 1) = ln(0.5/1.5 + 1) = ln(4/3)
        //   ≈ 0.2876820724517809
        // TF component:
        //   f=1, |D|=1, avgdl=1, k1=1.2, b=0.75
        //   denom = 1 + 1.2 * (1 - 0.75 + 0.75 * 1/1) = 1 + 1.2 * 1 = 2.2
        //   numerator = 1 * (1.2 + 1) = 2.2
        //   tf_component = 2.2 / 2.2 = 1.0
        // score = idf * tf_component = ln(4/3)
        let s = Bm25Scorer::from_docs(&[(1, "rust")]);
        let idf = s.idf("rust");
        assert!(approx(idf, (4.0_f64 / 3.0).ln()));
        let score = s.score(&["rust"], 1);
        assert!(approx(score, idf));
    }

    #[test]
    fn score_higher_tf_yields_higher_score_saturated() {
        // Increasing TF should increase the score but with saturation.
        let mut s_short = Bm25Scorer::new();
        s_short.add_document(1, &["rust"]);
        s_short.add_document(2, &["other"]);
        s_short.refresh_avg();

        let mut s_long = Bm25Scorer::new();
        s_long.add_document(1, &["rust", "rust", "rust", "rust", "rust"]);
        s_long.add_document(2, &["other"]);
        s_long.refresh_avg();

        let short_score = s_short.score(&["rust"], 1);
        let long_score = s_long.score(&["rust"], 1);
        // Higher TF should yield a higher score (saturation doesn't cap at 0).
        assert!(long_score > short_score);
    }

    #[test]
    fn rank_sorts_by_score_descending() {
        let docs = vec![
            (1u64, "rust rust rust database"),
            (2, "rust database"),
            (3, "database database database"),
            (4, "rust"),
        ];
        let scorer = Bm25Scorer::from_docs(&docs);
        let ranked = scorer.rank("rust", &[1, 2, 3, 4]);
        // All docs containing "rust" should appear; "database" doc 3 should
        // not.
        let ids: Vec<u64> = ranked.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(ids.contains(&4));
        assert!(!ids.contains(&3));
        // Scores should be non-increasing.
        for w in ranked.windows(2) {
            assert!(w[0].1 >= w[1].1);
        }
    }

    #[test]
    fn rank_empty_query_returns_empty() {
        let s = Bm25Scorer::from_docs(&[(1, "rust")]);
        assert!(s.rank("", &[1]).is_empty());
    }

    #[test]
    fn rank_only_stopwords_returns_empty() {
        let s = Bm25Scorer::from_docs(&[(1, "rust")]);
        assert!(s.rank("the and is", &[1]).is_empty());
    }

    #[test]
    fn rank_stems_query() {
        // "running" should stem to "run" and match indexed "run".
        let s = Bm25Scorer::from_docs(&[(1, "run fast")]);
        let ranked = s.rank("running", &[1]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 1);
        assert!(ranked[0].1 > 0.0);
    }

    #[test]
    fn rank_zero_scores_filtered() {
        let s = Bm25Scorer::from_docs(&[(1, "rust"), (2, "database")]);
        let ranked = s.rank("rust", &[1, 2]);
        // Only doc 1 matches.
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, 1);
    }

    #[test]
    fn score_sums_across_query_terms() {
        // Doc contains both "rust" and "alpha"; score should be sum of
        // both terms' contributions. (Using "alpha"/"beta" rather than
        // "database"/"engine" because the latter are Porter-stemmed to
        // "databas"/"engin" — we want raw terms that the scorer stores
        // verbatim.)
        let s = Bm25Scorer::from_docs(&[(1, "rust alpha"), (2, "beta"), (3, "beta")]);
        let score_both = s.score(&["rust", "alpha"], 1);
        let score_rust = s.score(&["rust"], 1);
        let score_alpha = s.score(&["alpha"], 1);
        assert!(approx(score_both, score_rust + score_alpha));
        assert!(score_both > score_rust);
        assert!(score_both > score_alpha);
    }

    #[test]
    fn repeated_query_terms_dont_double_score() {
        // BM25 uses unique query terms (the classic formulation treats a
        // query as a set of terms for the IDF × TF computation).
        let s = Bm25Scorer::from_docs(&[(1, "rust"), (2, "other")]);
        let once = s.score(&["rust"], 1);
        let twice = s.score(&["rust", "rust"], 1);
        assert!(approx(once, twice));
    }

    #[test]
    fn length_normalization_favors_shorter_docs() {
        // Two docs with the same TF for the query term; the shorter doc
        // should score higher (b > 0).
        let mut s = Bm25Scorer::new();
        s.add_document(1, &["rust", "other", "words", "here"]);
        s.add_document(2, &["rust", "filler", "filler", "filler", "filler", "filler", "filler", "filler", "filler"]);
        s.refresh_avg();
        let short = s.score(&["rust"], 1);
        let long = s.score(&["rust"], 2);
        assert!(
            short > long,
            "shorter doc with same TF should score higher (short={}, long={})",
            short, long
        );
    }

    #[test]
    fn b_zero_disables_length_normalization() {
        let mut s = Bm25Scorer::with_params(1.2, 0.0);
        s.add_document(1, &["rust", "other", "words", "here"]);
        s.add_document(2, &["rust", "filler", "filler", "filler", "filler", "filler", "filler", "filler", "filler"]);
        s.refresh_avg();
        let short = s.score(&["rust"], 1);
        let long = s.score(&["rust"], 2);
        // With b=0, both docs have tf=1 for "rust" so scores should be equal.
        assert!(approx(short, long));
    }

    #[test]
    fn rank_empty_doc_list_returns_empty() {
        let s = Bm25Scorer::from_docs(&[(1, "rust")]);
        assert!(s.rank("rust", &[]).is_empty());
    }

    #[test]
    fn rank_ties_break_by_doc_id_asc() {
        // Two docs with identical content — scores tie, lower doc_id first.
        let s = Bm25Scorer::from_docs(&[(5, "rust"), (3, "rust")]);
        let ranked = s.rank("rust", &[5, 3]);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0, 3);
        assert_eq!(ranked[1].0, 5);
        assert!(approx(ranked[0].1, ranked[1].1));
    }
}
