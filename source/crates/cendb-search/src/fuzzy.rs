//! Fuzzy matching via Levenshtein edit distance.
//!
//! Levenshtein distance is the minimum number of single-character edits
//! (insertions, deletions, substitutions) required to transform one string
//! into another. The distance is symmetric and satisfies the triangle
//! inequality.
//!
//! This module exposes:
//!   * [`levenshtein`] — standard O(m·n) dynamic-programming edit distance.
//!   * [`fuzzy_search`] — returns vocabulary terms within a distance budget,
//!     sorted by distance.
//!   * [`fuzzy_search_with_limit`] — same as `fuzzy_search` but capped at a
//!     maximum number of results.

/// Compute the Levenshtein edit distance between `a` and `b`.
///
/// The algorithm uses the standard two-row dynamic-programming approach,
/// running in O(m · n) time and O(min(m, n)) space, where m and n are the
/// character lengths of the inputs.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    levenshtein_chars(&a, &b)
}

fn levenshtein_chars(a: &[char], b: &[char]) -> usize {
    let m = a.len();
    let n = b.len();
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }

    // Keep two rows of the DP matrix.
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr: Vec<usize> = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let deletion = prev[j] + 1;
            let insertion = curr[j - 1] + 1;
            let substitution = prev[j - 1] + cost;
            curr[j] = deletion.min(insertion).min(substitution);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Search `vocabulary` for terms within `max_distance` edits of `term`.
///
/// Returns `(term, distance)` pairs sorted by distance ascending, then
/// alphabetically. Only terms with distance ≤ `max_distance` are returned.
/// The original `term` (if present in the vocabulary) is included with
/// distance 0.
pub fn fuzzy_search(term: &str, vocabulary: &[&str], max_distance: usize) -> Vec<(String, usize)> {
    fuzzy_search_impl(term, vocabulary, max_distance, usize::MAX)
}

/// Like [`fuzzy_search`] but caps the number of results at `limit`. Useful
/// when the vocabulary is large.
pub fn fuzzy_search_with_limit(
    term: &str,
    vocabulary: &[&str],
    max_distance: usize,
    limit: usize,
) -> Vec<(String, usize)> {
    fuzzy_search_impl(term, vocabulary, max_distance, limit)
}

fn fuzzy_search_impl(
    term: &str,
    vocabulary: &[&str],
    max_distance: usize,
    limit: usize,
) -> Vec<(String, usize)> {
    let mut results: Vec<(String, usize)> = vocabulary
        .iter()
        .map(|v| {
            let d = levenshtein(term, v);
            (v.to_string(), d)
        })
        .filter(|(_, d)| *d <= max_distance)
        .collect();
    // Sort by distance, then alphabetically for deterministic output.
    results.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    if limit < results.len() {
        results.truncate(limit);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_strings_distance_zero() {
        assert_eq!(levenshtein("hello", "hello"), 0);
        assert_eq!(levenshtein("", ""), 0);
    }

    #[test]
    fn empty_vs_nonempty() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn single_substitution() {
        assert_eq!(levenshtein("cat", "bat"), 1);
        assert_eq!(levenshtein("cat", "cot"), 1);
    }

    #[test]
    fn single_insertion() {
        assert_eq!(levenshtein("cat", "cats"), 1);
        assert_eq!(levenshtein("cat", "cart"), 1);
    }

    #[test]
    fn single_deletion() {
        assert_eq!(levenshtein("cats", "cat"), 1);
        assert_eq!(levenshtein("cart", "cat"), 1);
    }

    #[test]
    fn distance_two() {
        assert_eq!(levenshtein("cat", "dog"), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("sunday", "saturday"), 3);
    }

    #[test]
    fn case_sensitive() {
        assert_eq!(levenshtein("CAT", "cat"), 3);
        assert_eq!(levenshtein("Cat", "cat"), 1);
    }

    #[test]
    fn unicode_chars() {
        // Multi-byte chars counted as single units.
        assert_eq!(levenshtein("café", "cafe"), 1);
        assert_eq!(levenshtein("naïve", "naive"), 1);
        assert_eq!(levenshtein("日本語", "日本語"), 0);
        assert_eq!(levenshtein("日本語", "日本"), 1);
    }

    #[test]
    fn fuzzy_search_includes_exact_match_at_distance_0() {
        let vocab = ["cat", "car", "bat", "cart", "dog"];
        let results = fuzzy_search("cat", &vocab, 1);
        let distances: Vec<&str> = results.iter().map(|(s, _)| s.as_str()).collect();
        assert!(distances.contains(&"cat"));
        assert!(distances.contains(&"bat"));
        assert!(distances.contains(&"car"));
        // "cart" has distance 1 (insertion of 'r').
        assert!(distances.contains(&"cart"));
        // "dog" has distance 3 — excluded.
        assert!(!distances.contains(&"dog"));
    }

    #[test]
    fn fuzzy_search_distance_zero_only_returns_exact() {
        let vocab = ["cat", "car", "bat"];
        let results = fuzzy_search("cat", &vocab, 0);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "cat");
        assert_eq!(results[0].1, 0);
    }

    #[test]
    fn fuzzy_search_distance_one_includes_single_edits() {
        let vocab = ["cat", "car", "bat", "cats", "cart", "do", "dog", "frog"];
        let results = fuzzy_search("cat", &vocab, 1);
        let matched: std::collections::HashSet<&str> =
            results.iter().map(|(s, _)| s.as_str()).collect();
        // Within distance 1 of "cat":
        assert!(matched.contains("cat"));   // 0
        assert!(matched.contains("car"));   // 1 (sub)
        assert!(matched.contains("bat"));   // 1 (sub)
        assert!(matched.contains("cats"));  // 1 (ins)
        assert!(matched.contains("cart"));  // 1 (ins)
        // Excluded:
        assert!(!matched.contains("dog"));  // 3
        assert!(!matched.contains("frog")); // 4
        assert!(!matched.contains("do"));   // 2
    }

    #[test]
    fn fuzzy_search_distance_two_includes_more() {
        let vocab = ["cat", "dog", "do", "boat", "cart", "scat"];
        let results = fuzzy_search("cat", &vocab, 2);
        let matched: std::collections::HashSet<&str> =
            results.iter().map(|(s, _)| s.as_str()).collect();
        assert!(matched.contains("cat"));   // 0
        assert!(matched.contains("cart"));  // 1 (insert r)
        assert!(matched.contains("scat"));  // 1 (insert s)
        assert!(matched.contains("boat"));  // 2 (sub b→c, delete o)
        // "do" is distance 3 from "cat" (sub c→d, sub a→o, delete t).
        assert!(!matched.contains("do"));
        // "dog" is distance 3 from "cat".
        assert!(!matched.contains("dog"));
    }

    #[test]
    fn fuzzy_search_no_match_within_threshold() {
        let vocab = ["elephant", "rhinoceros", "hippopotamus"];
        let results = fuzzy_search("cat", &vocab, 2);
        assert!(results.is_empty());
    }

    #[test]
    fn fuzzy_search_sorted_by_distance_then_alpha() {
        let vocab = ["bat", "car", "cat"];
        let results = fuzzy_search("cat", &vocab, 1);
        // bat (1), car (1), cat (0) — sorted by distance then alpha.
        assert_eq!(results[0].0, "cat");
        assert_eq!(results[0].1, 0);
        assert_eq!(results[1].0, "bat");
        assert_eq!(results[1].1, 1);
        assert_eq!(results[2].0, "car");
        assert_eq!(results[2].1, 1);
    }

    #[test]
    fn fuzzy_search_with_limit_truncates() {
        let vocab = ["cat", "bat", "car", "cart", "cab", "can", "cap", "cay"];
        let results = fuzzy_search_with_limit("cat", &vocab, 1, 3);
        assert_eq!(results.len(), 3);
        // All results should be within distance 1.
        for (_, d) in &results {
            assert!(*d <= 1);
        }
    }

    #[test]
    fn fuzzy_search_empty_vocabulary() {
        let results = fuzzy_search("cat", &[], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn fuzzy_search_empty_term() {
        // The empty string is at distance n from any n-character word.
        let vocab = ["a", "ab", "abc", ""];
        let results = fuzzy_search("", &vocab, 2);
        let matched: std::collections::HashSet<&str> =
            results.iter().map(|(s, _)| s.as_str()).collect();
        assert!(matched.contains(""));
        assert!(matched.contains("a"));
        assert!(matched.contains("ab"));
        assert!(!matched.contains("abc")); // distance 3
    }

    #[test]
    fn fuzzy_search_handles_duplicates_in_vocab() {
        let vocab = ["cat", "cat", "bat"];
        let results = fuzzy_search("cat", &vocab, 1);
        // Duplicates are preserved (each is its own entry).
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn levenshtein_symmetric() {
        assert_eq!(levenshtein("abc", "xyz"), levenshtein("xyz", "abc"));
        assert_eq!(
            levenshtein("kitten", "sitting"),
            levenshtein("sitting", "kitten")
        );
    }
}
