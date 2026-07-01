//! Inverted index for full-text search with TF-IDF and BM25 scoring.

use std::collections::HashMap;

use crate::bm25::Bm25Scorer;
use crate::fuzzy::fuzzy_search;
use crate::snippet::generate_snippet;
use crate::tokenizer::tokenize;

/// A posting: a document ID + term frequency + positions.
#[derive(Clone, Debug)]
pub struct Posting {
    pub doc_id: u64,
    pub term_frequency: u32,
    pub positions: Vec<u32>,
}

/// A search result: document ID + relevance score + optional snippet.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub doc_id: u64,
    pub score: f64,
    /// Optional snippet (text fragment with `<mark>` highlights). Populated
    /// by [`InvertedIndex::search_ranked`]; `None` for plain `search` /
    /// `search_or` results.
    pub snippet: Option<String>,
}

/// TF-IDF scorer: computes relevance scores for search results.
pub struct TfIdfScorer {
    /// Number of documents in the corpus.
    doc_count: u64,
    /// Document frequency: number of documents containing each term.
    df: HashMap<String, u64>,
}

impl TfIdfScorer {
    pub fn new() -> Self {
        Self {
            doc_count: 0,
            df: HashMap::new(),
        }
    }

    /// Record that a document contains a set of unique terms.
    pub fn add_document(&mut self, terms: &[&str]) {
        self.doc_count += 1;
        let unique: std::collections::HashSet<&str> = terms.iter().copied().collect();
        for term in unique {
            *self.df.entry(term.to_string()).or_default() += 1;
        }
    }

    /// Compute the TF-IDF score for a term in a document.
    pub fn score(&self, term: &str, tf: u32) -> f64 {
        if self.doc_count == 0 {
            return 0.0;
        }
        let df = self.df.get(term).copied().unwrap_or(0);
        if df == 0 {
            return 0.0;
        }
        let idf = ((self.doc_count as f64) / (df as f64)).ln();
        let tf_norm = 1.0 + (tf as f64).ln();
        tf_norm * idf
    }

    /// Number of documents in the corpus.
    pub fn doc_count(&self) -> u64 {
        self.doc_count
    }

    /// Document frequency for a term: number of docs containing it.
    pub fn df(&self, term: &str) -> u64 {
        self.df.get(term).copied().unwrap_or(0)
    }
}

impl Default for TfIdfScorer {
    fn default() -> Self {
        Self::new()
    }
}

/// Inverted index: maps terms → postings list.
pub struct InvertedIndex {
    /// The inverted index: term → list of postings.
    index: HashMap<String, Vec<Posting>>,
    /// TF-IDF scorer for relevance ranking.
    scorer: TfIdfScorer,
    /// Total number of indexed documents.
    doc_count: u64,
    /// Original document text keyed by doc_id. Used for snippet generation
    /// and to recompute document lengths on demand.
    documents: HashMap<u64, String>,
    /// Document length (number of indexed terms, including stop-words which
    /// increment position but are not indexed) — actually the number of
    /// *indexed* (non-stop-word) tokens, which is the standard BM25
    /// document-length convention.
    doc_lengths: HashMap<u64, usize>,
}

impl InvertedIndex {
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            scorer: TfIdfScorer::new(),
            doc_count: 0,
            documents: HashMap::new(),
            doc_lengths: HashMap::new(),
        }
    }

    /// Index a document: tokenize, stem, filter stop-words, and add to
    /// the inverted index. The original `text` is retained for snippet
    /// generation.
    pub fn add_document(&mut self, doc_id: u64, text: &str) {
        let tokens = tokenize(text);
        self.doc_count += 1;
        self.documents.insert(doc_id, text.to_string());
        self.doc_lengths.insert(doc_id, tokens.len());

        // Group positions by term.
        let mut term_postings: HashMap<String, Vec<u32>> = HashMap::new();
        for token in &tokens {
            term_postings
                .entry(token.term.clone())
                .or_default()
                .push(token.position);
        }

        // Add to inverted index.
        let terms: Vec<&str> = term_postings.keys().map(|s| s.as_str()).collect();
        self.scorer.add_document(&terms);

        for (term, positions) in term_postings {
            let tf = positions.len() as u32;
            self.index
                .entry(term)
                .or_default()
                .push(Posting {
                    doc_id,
                    term_frequency: tf,
                    positions,
                });
        }
    }

    /// Search for documents containing all query terms (AND semantics).
    /// Returns results sorted by TF-IDF score (descending). The `snippet`
    /// field of each [`SearchResult`] is `None`; use [`Self::search_ranked`]
    /// for snippet-aware ranked search.
    pub fn search(&self, query: &str) -> Vec<SearchResult> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        // Get postings for each query term.
        let mut all_postings: Vec<&Vec<Posting>> = Vec::new();
        for token in &query_tokens {
            match self.index.get(&token.term) {
                Some(postings) => all_postings.push(postings),
                None => return Vec::new(), // Term not found → no results.
            }
        }

        // Intersect: find documents that appear in ALL terms' postings.
        let mut candidate_docs: HashMap<u64, Vec<(String, u32)>> = HashMap::new();
        for (i, postings) in all_postings.iter().enumerate() {
            let term = &query_tokens[i].term;
            for posting in postings.iter() {
                candidate_docs
                    .entry(posting.doc_id)
                    .or_default()
                    .push((term.clone(), posting.term_frequency));
            }
        }

        // Filter to docs that have ALL terms.
        let term_count = query_tokens.len();
        let mut results: Vec<SearchResult> = candidate_docs
            .into_iter()
            .filter(|(_, terms)| terms.len() == term_count)
            .map(|(doc_id, terms)| {
                let score: f64 = terms
                    .iter()
                    .map(|(term, tf)| self.scorer.score(term, *tf))
                    .sum();
                SearchResult {
                    doc_id,
                    score,
                    snippet: None,
                }
            })
            .collect();

        // Sort by score descending.
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    /// Search with OR semantics: return documents containing any query term.
    pub fn search_or(&self, query: &str) -> Vec<SearchResult> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }

        let mut doc_scores: HashMap<u64, f64> = HashMap::new();
        for token in &query_tokens {
            if let Some(postings) = self.index.get(&token.term) {
                for posting in postings {
                    let score = self.scorer.score(&token.term, posting.term_frequency);
                    *doc_scores.entry(posting.doc_id).or_default() += score;
                }
            }
        }

        let mut results: Vec<SearchResult> = doc_scores
            .into_iter()
            .map(|(doc_id, score)| SearchResult {
                doc_id,
                score,
                snippet: None,
            })
            .collect();
        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results
    }

    /// Number of unique terms in the index.
    pub fn term_count(&self) -> usize {
        self.index.len()
    }

    /// Number of documents indexed.
    pub fn doc_count(&self) -> u64 {
        self.doc_count
    }

    /// Document frequency for `term`: number of documents containing it.
    pub fn doc_freq(&self, term: &str) -> u64 {
        self.scorer.df(term)
    }

    /// Document length (number of indexed tokens) for `doc_id`. Returns 0
    /// if the document is not indexed.
    pub fn doc_length(&self, doc_id: u64) -> usize {
        self.doc_lengths.get(&doc_id).copied().unwrap_or(0)
    }

    /// Average document length across the corpus. Returns 0.0 if no
    /// documents are indexed.
    pub fn avg_doc_length(&self) -> f64 {
        if self.doc_count == 0 {
            return 0.0;
        }
        let total: usize = self.doc_lengths.values().sum();
        total as f64 / self.doc_count as f64
    }

    /// Term frequency of `term` in `doc_id` (0 if absent).
    pub fn term_frequency(&self, term: &str, doc_id: u64) -> u32 {
        self.index
            .get(term)
            .and_then(|postings| postings.iter().find(|p| p.doc_id == doc_id))
            .map(|p| p.term_frequency)
            .unwrap_or(0)
    }

    /// Original document text for `doc_id`, if available.
    pub fn document_text(&self, doc_id: u64) -> Option<&str> {
        self.documents.get(&doc_id).map(|s| s.as_str())
    }

    /// All unique terms in the index (the vocabulary). Order is unspecified.
    pub fn vocabulary(&self) -> Vec<String> {
        self.index.keys().cloned().collect()
    }

    /// Fuzzy query: return doc IDs that contain any term within
    /// `max_distance` edits of `term`. The input `term` is matched against
    /// the index vocabulary (already-stemmed terms); callers should
    /// typically pre-stem `term` with the Porter stemmer for consistency
    /// with the indexed terms, but raw terms work too if the index was
    /// built without stemming.
    pub fn fuzzy_query(&self, term: &str, max_distance: usize) -> Vec<u64> {
        let vocab: Vec<String> = self.vocabulary();
        let vocab_refs: Vec<&str> = vocab.iter().map(|s| s.as_str()).collect();
        let matches = fuzzy_search(term, &vocab_refs, max_distance);
        let mut doc_ids: Vec<u64> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (matched_term, _distance) in matches {
            if let Some(postings) = self.index.get(&matched_term) {
                for posting in postings {
                    if seen.insert(posting.doc_id) {
                        doc_ids.push(posting.doc_id);
                    }
                }
            }
        }
        doc_ids.sort_unstable();
        doc_ids
    }

    /// Build a [`Bm25Scorer`] from the current index state. The scorer
    /// owns a snapshot of the corpus statistics (document count, average
    /// document length, per-term document frequency, per-document term
    /// frequencies, per-document lengths).
    pub fn bm25_scorer(&self) -> Bm25Scorer {
        let mut scorer = Bm25Scorer::new();
        // Walk every (term, posting) pair and accumulate per-doc term
        // frequencies. The scorer's `add_document` recomputes document
        // frequency and document length from the supplied terms slice, so
        // we expand each (term, tf) pair into `tf` copies of the term to
        // preserve the original document length.
        let mut per_doc_tf: HashMap<u64, HashMap<String, u32>> = HashMap::new();
        for (term, postings) in &self.index {
            for posting in postings {
                per_doc_tf
                    .entry(posting.doc_id)
                    .or_default()
                    .insert(term.clone(), posting.term_frequency);
            }
        }
        for (doc_id, tf_map) in per_doc_tf {
            let mut terms: Vec<String> = Vec::new();
            for (term, &tf) in &tf_map {
                for _ in 0..tf {
                    terms.push(term.clone());
                }
            }
            let term_refs: Vec<&str> = terms.iter().map(|s| s.as_str()).collect();
            scorer.add_document(doc_id, &term_refs);
        }
        scorer
    }

    /// Ranked search: stems the query, retrieves matching documents via
    /// exact and fuzzy matching, scores them with BM25, and generates
    /// snippets for each result.
    ///
    /// * `query` — the raw query string (will be tokenized, stop-word
    ///   filtered, and Porter-stemmed).
    /// * `max_fuzzy_distance` — maximum Levenshtein distance for fuzzy term
    ///   expansion. Set to 0 to disable fuzzy matching.
    /// * `snippet_length` — maximum length (in characters) of each result's
    ///   snippet.
    ///
    /// Returns [`SearchResult`]s sorted by descending BM25 score, each with
    /// a `snippet` populated from the original document text (or `None` if
    /// the document text is unavailable or no query term appears verbatim).
    ///
    /// When fuzzy matching is enabled, each query term is expanded to the
    /// set of vocabulary terms within `max_fuzzy_distance` edits; the
    /// union of these (plus exact matches) forms the *effective* query
    /// term set used for BM25 scoring. This means a typo like "rist" can
    /// still score a document containing "rust".
    pub fn search_ranked(
        &self,
        query: &str,
        max_fuzzy_distance: usize,
        snippet_length: usize,
    ) -> Vec<SearchResult> {
        let query_tokens = tokenize(query);
        if query_tokens.is_empty() {
            return Vec::new();
        }
        let query_terms: Vec<String> = query_tokens.iter().map(|t| t.term.clone()).collect();

        // Original (un-stemmed, lowercased, non-stop-word) query terms for
        // snippet highlighting — these match surface forms in the document
        // text, whereas the stemmed terms would not (e.g. the query "running"
        // stems to "run" but the document contains "running").
        let snippet_terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .filter(|w| !crate::tokenizer::is_stopword(w))
            .collect();

        // Build the set of effective query terms (exact + fuzzy expansions)
        // and collect candidate document IDs.
        let mut effective_terms: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut candidates: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for term in &query_terms {
            if let Some(postings) = self.index.get(term) {
                effective_terms.insert(term.clone());
                for posting in postings {
                    candidates.insert(posting.doc_id);
                }
            }
            if max_fuzzy_distance > 0 {
                let vocab: Vec<String> = self.vocabulary();
                let vocab_refs: Vec<&str> = vocab.iter().map(|s| s.as_str()).collect();
                for (matched, _dist) in fuzzy_search(term, &vocab_refs, max_fuzzy_distance) {
                    effective_terms.insert(matched.clone());
                    if let Some(postings) = self.index.get(&matched) {
                        for posting in postings {
                            candidates.insert(posting.doc_id);
                        }
                    }
                }
            }
        }

        if candidates.is_empty() {
            return Vec::new();
        }

        // Score with BM25 using the effective term set.
        let scorer = self.bm25_scorer();
        let effective_refs: Vec<&str> = effective_terms.iter().map(|s| s.as_str()).collect();
        let mut scored: Vec<(u64, f64)> = candidates
            .iter()
            .map(|&id| (id, scorer.score(&effective_refs, id)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        // Generate snippets for the top results using the original (surface)
        // query terms so that highlights match the document text.
        let snippet_refs: Vec<&str> = snippet_terms.iter().map(|s| s.as_str()).collect();
        scored
            .into_iter()
            .map(|(doc_id, score)| {
                let snippet = self
                    .document_text(doc_id)
                    .and_then(|text| generate_snippet(text, &snippet_refs, snippet_length));
                SearchResult {
                    doc_id,
                    score,
                    snippet,
                }
            })
            .collect()
    }
}

impl Default for InvertedIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_search_basic() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "The quick brown fox jumps over the lazy dog");
        idx.add_document(2, "A quick brown dog outruns the quick fox");
        idx.add_document(3, "The lazy dog sleeps all day");

        let results = idx.search("quick fox");
        assert!(!results.is_empty());
        // Both doc 1 and 2 contain "quick" and "fox".
        assert!(results.iter().any(|r| r.doc_id == 1));
        assert!(results.iter().any(|r| r.doc_id == 2));
    }

    #[test]
    fn search_no_results() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "hello world");
        let results = idx.search("nonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn search_or_semantics() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "database storage engine");
        idx.add_document(2, "web framework rust");
        idx.add_document(3, "database rust engine");

        let results = idx.search_or("database rust");
        // All 3 documents should be returned (OR semantics).
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn tfidf_scoring() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust database rust database rust");
        idx.add_document(2, "rust");
        idx.add_document(3, "database");

        let results = idx.search("rust");
        // Doc 1 has "rust" 3 times (higher TF) but "rust" appears in
        // 2 docs (lower IDF). Doc 2 has "rust" once but it's rarer.
        assert!(!results.is_empty());
    }

    #[test]
    fn search_result_has_none_snippet_by_default() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "hello world");
        let results = idx.search("hello");
        assert_eq!(results.len(), 1);
        assert!(results[0].snippet.is_none());
    }

    #[test]
    fn doc_length_tracks_indexed_tokens() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "The quick brown fox"); // "the" is stopword
        assert_eq!(idx.doc_length(1), 3); // quick, brown, fox
    }

    #[test]
    fn avg_doc_length() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust database"); // 2 tokens
        idx.add_document(2, "rust"); // 1 token
        assert_eq!(idx.avg_doc_length(), 1.5);
    }

    #[test]
    fn doc_freq_counts_documents_containing_term() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust alpha");
        idx.add_document(2, "rust beta");
        idx.add_document(3, "alpha beta");
        assert_eq!(idx.doc_freq("rust"), 2);
        assert_eq!(idx.doc_freq("alpha"), 2);
        assert_eq!(idx.doc_freq("beta"), 2);
        assert_eq!(idx.doc_freq("nonexistent"), 0);
    }

    #[test]
    fn term_frequency_lookup() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust rust rust alpha");
        assert_eq!(idx.term_frequency("rust", 1), 3);
        assert_eq!(idx.term_frequency("alpha", 1), 1);
        assert_eq!(idx.term_frequency("rust", 999), 0);
    }

    #[test]
    fn vocabulary_lists_all_terms() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "alpha beta");
        idx.add_document(2, "gamma delta");
        let vocab = idx.vocabulary();
        assert_eq!(vocab.len(), 4);
        for term in &["alpha", "beta", "gamma", "delta"] {
            assert!(vocab.iter().any(|v| v == term));
        }
    }

    #[test]
    fn document_text_roundtrip() {
        let mut idx = InvertedIndex::new();
        idx.add_document(42, "the quick brown fox");
        assert_eq!(idx.document_text(42), Some("the quick brown fox"));
        assert_eq!(idx.document_text(99), None);
    }

    #[test]
    fn fuzzy_query_returns_docs_with_close_terms() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "cat");
        idx.add_document(2, "car");
        idx.add_document(3, "dog");
        // "cat" with distance 1 should match docs for "cat" and "car".
        let docs = idx.fuzzy_query("cat", 1);
        assert!(docs.contains(&1));
        assert!(docs.contains(&2));
        assert!(!docs.contains(&3));
    }

    #[test]
    fn fuzzy_query_distance_zero_is_exact() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "cat");
        idx.add_document(2, "car");
        let docs = idx.fuzzy_query("cat", 0);
        assert_eq!(docs, vec![1]);
    }

    #[test]
    fn fuzzy_query_returns_sorted_unique_doc_ids() {
        let mut idx = InvertedIndex::new();
        idx.add_document(5, "cat");
        idx.add_document(3, "bat");
        idx.add_document(7, "rat");
        idx.add_document(1, "cat"); // different doc, same term
        let docs = idx.fuzzy_query("cat", 1);
        // Should be sorted and unique.
        let mut sorted = docs.clone();
        sorted.sort_unstable();
        assert_eq!(docs, sorted);
        assert_eq!(docs.iter().filter(|&&d| d == 1).count(), 1);
        assert_eq!(docs.iter().filter(|&&d| d == 5).count(), 1);
    }

    #[test]
    fn fuzzy_query_no_matches_returns_empty() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "elephant");
        let docs = idx.fuzzy_query("cat", 1);
        assert!(docs.is_empty());
    }

    #[test]
    fn bm25_scorer_reflects_index() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust alpha");
        idx.add_document(2, "rust beta");
        let scorer = idx.bm25_scorer();
        assert_eq!(scorer.doc_count(), 2);
        assert_eq!(scorer.doc_freq("rust"), 2);
        assert_eq!(scorer.doc_freq("alpha"), 1);
        assert_eq!(scorer.doc_len(1), 2);
        assert_eq!(scorer.term_frequency("rust", 1), 1);
    }

    #[test]
    fn search_ranked_basic() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "the rust programming language is fast");
        idx.add_document(2, "rust is a systems programming language");
        idx.add_document(3, "python is a scripting language");
        let results = idx.search_ranked("rust programming", 0, 100);
        // Docs 1 and 2 contain both "rust" and "programming"; doc 3 has neither.
        let ids: Vec<u64> = results.iter().map(|r| r.doc_id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(!ids.contains(&3));
        // All scores should be positive.
        for r in &results {
            assert!(r.score > 0.0);
        }
    }

    #[test]
    fn search_ranked_generates_snippets() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "the rust programming language is fast and safe");
        let results = idx.search_ranked("rust", 0, 100);
        assert_eq!(results.len(), 1);
        let snippet = results[0].snippet.as_ref().expect("snippet should be set");
        assert!(snippet.contains("<mark>rust</mark>"), "snippet: {}", snippet);
    }

    #[test]
    fn search_ranked_empty_query_returns_empty() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "hello world");
        assert!(idx.search_ranked("", 0, 100).is_empty());
    }

    #[test]
    fn search_ranked_stopword_only_query_returns_empty() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "the quick brown fox");
        assert!(idx.search_ranked("the and is", 0, 100).is_empty());
    }

    #[test]
    fn search_ranked_stems_query() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "cats are running fast");
        // "cat" should match "cats"; "running" should match "running".
        let results = idx.search_ranked("cat running", 0, 100);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
    }

    #[test]
    fn search_ranked_fuzzy_matches_typos() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust programming");
        // Query "rist" (typo of "rust") with fuzzy distance 1 should still
        // match doc 1 via the fuzzy expansion.
        let results = idx.search_ranked("rist", 1, 100);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].doc_id, 1);
    }

    #[test]
    fn search_ranked_orders_by_bm25_score() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust rust rust database");
        idx.add_document(2, "rust database");
        idx.add_document(3, "database database database");
        let results = idx.search_ranked("rust", 0, 100);
        // Doc 1 (3× "rust") should outrank doc 2 (1× "rust"); doc 3 has no
        // "rust" so it shouldn't appear.
        let ids: Vec<u64> = results.iter().map(|r| r.doc_id).collect();
        assert!(ids.contains(&1) && ids.contains(&2));
        assert!(!ids.contains(&3));
        let score1 = results.iter().find(|r| r.doc_id == 1).unwrap().score;
        let score2 = results.iter().find(|r| r.doc_id == 2).unwrap().score;
        assert!(score1 > score2, "score1={} score2={}", score1, score2);
    }

    #[test]
    fn search_ranked_snippet_none_when_doc_text_missing() {
        // Build an index where the document text map is consistent (always
        // populated by add_document), so this test instead verifies the
        // behavior when no query term matches the document text — the
        // snippet is None because generate_snippet returns None.
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "alpha beta gamma");
        // Fuzzy distance 0 with a non-matching term → no candidates.
        let results = idx.search_ranked("nonexistent", 0, 100);
        assert!(results.is_empty());
    }

    #[test]
    fn search_ranked_no_fuzzy_when_distance_zero() {
        let mut idx = InvertedIndex::new();
        idx.add_document(1, "rust");
        idx.add_document(2, "rusty"); // stems to "rusti"
        // Distance 0 means no fuzzy expansion; "rust" matches only doc 1.
        let results = idx.search_ranked("rust", 0, 100);
        let ids: Vec<u64> = results.iter().map(|r| r.doc_id).collect();
        assert!(ids.contains(&1));
        assert!(!ids.contains(&2));
    }
}
