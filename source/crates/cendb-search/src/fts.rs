//! Inverted index for full-text search with TF-IDF scoring.

use std::collections::HashMap;

use crate::tokenizer::tokenize;

/// A posting: a document ID + term frequency + positions.
#[derive(Clone, Debug)]
pub struct Posting {
    pub doc_id: u64,
    pub term_frequency: u32,
    pub positions: Vec<u32>,
}

/// A search result: document ID + relevance score.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub doc_id: u64,
    pub score: f64,
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
}

impl InvertedIndex {
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            scorer: TfIdfScorer::new(),
            doc_count: 0,
        }
    }

    /// Index a document: tokenize, stem, filter stop-words, and add to
    /// the inverted index.
    pub fn add_document(&mut self, doc_id: u64, text: &str) {
        let tokens = tokenize(text);
        self.doc_count += 1;

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
    /// Returns results sorted by TF-IDF score (descending).
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
                SearchResult { doc_id, score }
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
            .map(|(doc_id, score)| SearchResult { doc_id, score })
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
}
