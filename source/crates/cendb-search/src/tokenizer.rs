//! Tokenizer, stemmer, and stop-word filter for full-text search.

/// A token: the stemmed form of a word, with its position in the source.
#[derive(Clone, Debug)]
pub struct Token {
    /// The stemmed term (lowercase).
    pub term: String,
    /// Position in the original text (word index, 0-based).
    pub position: u32,
}

/// Tokenize text: split on whitespace/punctuation, lowercase, filter
/// stop-words, and apply Porter-stemming (simplified).
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position = 0u32;

    for word in text.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let lower = word.to_lowercase();
        if is_stopword(&lower) {
            position += 1;
            continue;
        }
        let stemmed = stem(&lower);
        tokens.push(Token {
            term: stemmed,
            position,
        });
        position += 1;
    }
    tokens
}

/// English stop-words list (common words that are filtered out).
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "is", "are", "was", "were",
    "be", "been", "being", "have", "has", "had", "do", "does", "did",
    "will", "would", "could", "should", "may", "might", "must", "can",
    "this", "that", "these", "those", "i", "you", "he", "she", "it",
    "we", "they", "what", "which", "who", "when", "where", "why", "how",
    "all", "each", "every", "both", "few", "more", "most", "other",
    "some", "such", "no", "nor", "not", "only", "own", "same", "so",
    "than", "too", "very", "s", "t", "just", "don", "now", "in", "on",
    "at", "to", "for", "of", "with", "by", "from", "up", "about", "into",
    "through", "during", "before", "after", "above", "below", "between",
];

/// Check if a word is a stop-word.
pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

/// Simplified Porter stemmer: strips common English suffixes.
/// This is not a full Porter stemmer but covers the most common cases.
pub fn stem(word: &str) -> String {
    // Delegate to the full Porter stemmer in `crate::stemmer` for
    // consistency between indexed terms and query terms.
    crate::stemmer::stem(word)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_basic() {
        let tokens = tokenize("The quick brown fox jumps over the lazy dog");
        // "the" (x2) is a stop-word → filtered out.
        assert_eq!(tokens.len(), 7); // quick, brown, fox, jump, over, lazy, dog
        assert_eq!(tokens[0].term, "quick");
        assert_eq!(tokens[2].term, "fox");
    }

    #[test]
    fn tokenize_with_punctuation() {
        let tokens = tokenize("Hello, world! This is a test.");
        assert!(tokens.iter().any(|t| t.term == "hello"));
        assert!(tokens.iter().any(|t| t.term == "world"));
        assert!(tokens.iter().any(|t| t.term == "test"));
    }

    #[test]
    fn stem_plural() {
        assert_eq!(stem("cats"), "cat");
        assert_eq!(stem("ponies"), "poni");
        assert_eq!(stem("caresses"), "caress");
        assert_eq!(stem("cat"), "cat");
    }

    #[test]
    fn stem_past_tense() {
        assert_eq!(stem("plastered"), "plaster");
        assert_eq!(stem("motoring"), "motor");
    }

    #[test]
    fn stopword_filtering() {
        assert!(is_stopword("the"));
        assert!(is_stopword("is"));
        assert!(!is_stopword("database"));
    }
}
