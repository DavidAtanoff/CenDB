//! cendb-search: full-text search and secondary indexing.
//!
//! ## Full-Text Search (FTS)
//!
//! Implements an inverted index with:
//!   * Tokenization (split on whitespace/punctuation).
//!   * Stemming (Porter stemmer — simplified).
//!   * Stop-word filtering.
//!   * TF-IDF scoring.
//!
//! ## Secondary Indexes
//!
//! Secondary indexes map column values → RowLocator. They point to
//! **physical RowLocators** (not logical keys), requiring index updates
//! when blocks are compacted. Index updates are piggybacked on MVCC
//! commit: the write-set includes both primary and secondary index entries.

pub mod fts;
pub mod secondary;
pub mod tokenizer;

pub use fts::{InvertedIndex, SearchResult, TfIdfScorer};
pub use secondary::{SecondaryIndex, SecondaryIndexEntry};
pub use tokenizer::{tokenize, stem, is_stopword, Token};
