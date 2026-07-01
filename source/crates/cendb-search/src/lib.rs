//! cendb-search: full-text search and secondary indexing.
//!
//! ## Full-Text Search (FTS)
//!
//! Implements an inverted index with:
//!   * Tokenization (split on whitespace/punctuation).
//!   * Stemming (Porter stemmer — full 5-step algorithm; see [`stemmer`]).
//!   * Stop-word filtering.
//!   * TF-IDF scoring (see [`fts::TfIdfScorer`]) and BM25 ranking
//!     (see [`bm25::Bm25Scorer`]).
//!   * Fuzzy matching via Levenshtein edit distance (see [`fuzzy`]).
//!   * Snippet generation for search results (see [`snippet`]).
//!
//! Ranked search is exposed via [`InvertedIndex::search_ranked`], which
//! combines stemming, exact + fuzzy retrieval, BM25 scoring, and snippet
//! generation into a single call.
//!
//! ## Secondary Indexes
//!
//! Secondary indexes map column values → RowLocator. They point to
//! **physical RowLocators** (not logical keys), requiring index updates
//! when blocks are compacted. Index updates are piggybacked on MVCC
//! commit: the write-set includes both primary and secondary index entries.

pub mod bm25;
pub mod fts;
pub mod fuzzy;
pub mod secondary;
pub mod snippet;
pub mod stemmer;
pub mod tokenizer;

pub use bm25::Bm25Scorer;
pub use fts::{InvertedIndex, Posting, SearchResult, TfIdfScorer};
pub use fuzzy::{fuzzy_search, fuzzy_search_with_limit, levenshtein};
pub use secondary::{SecondaryIndex, SecondaryIndexEntry};
pub use snippet::generate_snippet;
pub use stemmer::{stem as porter_stem, stem_owned, stem_text};
pub use tokenizer::{is_stopword, stem, tokenize, Token};
