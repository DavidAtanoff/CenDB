//! Snippet (search-result fragment) generation.
//!
//! Given a document's text and a set of query terms, produces a short
//! fragment centred on the densest cluster of matches. Matched terms are
//! wrapped in `<mark>...</mark>` tags and the snippet is truncated with
//! ellipses (`...`) when it doesn't start/end at a document boundary.

/// A single match: byte offset range `[start, end)` in the source text plus
/// the term that was matched.
struct Match {
    start: usize,
    end: usize,
}

/// Generate a snippet from `text` centred on the densest window of query-term
/// matches.
///
/// * `text` — the full document text.
/// * `query_terms` — terms to highlight (matched case-insensitively as whole
///   alphanumeric runs; terms need not be stemmed — matching is on the raw
///   substring, but word boundaries are respected).
/// * `max_length` — maximum length (in characters) of the snippet content
///   excluding `<mark>` tags and leading/trailing `...` markers.
///
/// Returns `None` when `text` is empty, `max_length` is zero, or no query
/// term matches. Otherwise returns a snippet string with `<mark>` tags around
/// each match and `...` affixes when truncated.
pub fn generate_snippet(text: &str, query_terms: &[&str], max_length: usize) -> Option<String> {
    if text.is_empty() || max_length == 0 || query_terms.is_empty() {
        return None;
    }

    let matches = find_matches(text, query_terms);
    if matches.is_empty() {
        return None;
    }

    let text_len = text.chars().count();
    let max_length = max_length.min(text_len);

    // Find the window [win_start, win_start + max_length) (in char offsets)
    // that contains the most match starts.
    let mut best_start = 0usize;
    let mut best_count = 0usize;
    // Convert match offsets to char-space once.
    let char_matches: Vec<Match> = matches;
    let char_starts: Vec<usize> = char_matches.iter().map(|m| m.start).collect();

    // Sliding window: for each possible window start, count matches whose
    // start falls within [start, start + max_length). We iterate window
    // starts that align with each match start (the optimal window always
    // starts at a match boundary).
    for &start in char_starts.iter() {
        let win_end = start + max_length;
        let count = char_starts.iter().filter(|&&s| s >= start && s < win_end).count();
        if count > best_count {
            best_count = count;
            best_start = start;
        }
    }

    // Also consider starting at 0 (so we don't miss a leading cluster).
    {
        let win_end = max_length;
        let count = char_starts.iter().filter(|&&s| s < win_end).count();
        if count > best_count {
            best_count = count;
            best_start = 0;
        }
    }

    let _ = best_count;

    let win_end = (best_start + max_length).min(text_len);

    // Now extract the substring [best_start, win_end) in char space and
    // wrap any matches that fall within the window in <mark> tags.
    let snippet = build_snippet(text, best_start, win_end, &char_matches);

    let prefix = if best_start > 0 { "..." } else { "" };
    let suffix = if win_end < text_len { "..." } else { "" };
    Some(format!("{}{}{}", prefix, snippet, suffix))
}

/// Find all whole-word, case-insensitive matches of `query_terms` in `text`.
/// Returns matches with offsets measured in *character* (not byte) units.
fn find_matches(text: &str, query_terms: &[&str]) -> Vec<Match> {
    // Lowercase the query terms once.
    let needles: Vec<String> = query_terms
        .iter()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if needles.is_empty() {
        return Vec::new();
    }

    // Walk through `text` one alphanumeric word at a time, recording matches
    // by character offset.
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut matches = Vec::new();
    let mut i = 0;
    while i < n {
        if chars[i].is_alphanumeric() {
            let start = i;
            while i < n && chars[i].is_alphanumeric() {
                i += 1;
            }
            let end = i; // exclusive
            let word: String = chars[start..end].iter().collect();
            let lower = word.to_lowercase();
            if needles.iter().any(|n| *n == lower) {
                matches.push(Match { start, end });
            }
        } else {
            i += 1;
        }
    }
    matches
}

/// Build the snippet string for the window `[win_start, win_end)` (in char
/// offsets) with `<mark>` tags around matches overlapping the window.
fn build_snippet(text: &str, win_start: usize, win_end: usize, matches: &[Match]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = win_start;
    while i < win_end {
        // Check if a match starts at position i.
        let m = matches.iter().find(|m| m.start == i);
        if let Some(m) = m {
            // Output the marked segment, clamped to the window end.
            let m_end = m.end.min(win_end);
            out.push_str("<mark>");
            for c in &chars[i..m_end] {
                out.push(*c);
            }
            out.push_str("</mark>");
            i = m_end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_text_returns_none() {
        assert_eq!(generate_snippet("", &["foo"], 50), None);
    }

    #[test]
    fn no_query_terms_returns_none() {
        assert_eq!(generate_snippet("hello world", &[], 50), None);
    }

    #[test]
    fn zero_max_length_returns_none() {
        assert_eq!(generate_snippet("hello world", &["hello"], 0), None);
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(
            generate_snippet("hello world", &["nonexistent"], 50),
            None
        );
    }

    #[test]
    fn basic_snippet_with_highlight() {
        let text = "The quick brown fox jumps over the lazy dog";
        let s = generate_snippet(text, &["fox"], 100).unwrap();
        assert!(s.contains("<mark>fox</mark>"), "snippet was: {}", s);
    }

    #[test]
    fn snippet_is_case_insensitive() {
        let text = "The QUICK brown fox";
        let s = generate_snippet(text, &["quick"], 100).unwrap();
        assert!(s.contains("<mark>QUICK</mark>"), "snippet was: {}", s);
    }

    #[test]
    fn snippet_matches_whole_words_only() {
        // "cat" should not match inside "concatenate".
        let text = "concatenate the strings";
        assert_eq!(generate_snippet(text, &["cat"], 100), None);
    }

    #[test]
    fn snippet_truncates_with_ellipsis_when_text_longer() {
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let s = generate_snippet(text, &["epsilon"], 20).unwrap();
        assert!(
            s.starts_with("..."),
            "snippet should start with ...: {}",
            s
        );
        assert!(s.ends_with("..."), "snippet should end with ...: {}", s);
        assert!(s.contains("<mark>epsilon</mark>"));
    }

    #[test]
    fn snippet_no_prefix_ellipsis_when_at_start() {
        let text = "alpha beta gamma delta epsilon";
        let s = generate_snippet(text, &["alpha"], 100).unwrap();
        assert!(
            !s.starts_with("..."),
            "snippet should not start with ...: {}",
            s
        );
    }

    #[test]
    fn snippet_no_suffix_ellipsis_when_at_end() {
        let text = "alpha beta gamma delta epsilon";
        let s = generate_snippet(text, &["epsilon"], 100).unwrap();
        assert!(
            !s.ends_with("..."),
            "snippet should not end with ...: {}",
            s
        );
    }

    #[test]
    fn snippet_picks_densest_window() {
        // Two clusters of matches; the denser cluster should be selected.
        let text = "foo bar foo bar baz baz baz baz baz baz baz baz baz baz baz end";
        // "foo" appears near the start; "baz" cluster near the end.
        let s = generate_snippet(text, &["baz"], 20).unwrap();
        // The snippet should be from the baz cluster (after the foos).
        assert!(s.contains("<mark>baz</mark>"));
        // It should not contain "foo" (the sparse cluster).
        assert!(!s.contains("foo"), "snippet was: {}", s);
    }

    #[test]
    fn snippet_highlights_multiple_matches() {
        let text = "the cat sat on the mat";
        let s = generate_snippet(text, &["cat", "mat"], 100).unwrap();
        assert!(s.contains("<mark>cat</mark>"), "snippet was: {}", s);
        assert!(s.contains("<mark>mat</mark>"), "snippet was: {}", s);
    }

    #[test]
    fn snippet_highlights_only_matches_in_window() {
        // Long text where the second match is outside the chosen window.
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let s = generate_snippet(text, &["beta", "kappa"], 20).unwrap();
        // The chosen window should contain either beta or kappa, not both.
        let has_beta = s.contains("<mark>beta</mark>");
        let has_kappa = s.contains("<mark>kappa</mark>");
        assert!(
            has_beta ^ has_kappa,
            "expected exactly one match in window, snippet: {}",
            s
        );
    }

    #[test]
    fn snippet_max_length_smaller_than_word() {
        // The query term is longer than max_length; the matched window will
        // be clamped to max_length and only a prefix of the word shown.
        let text = "a longword here";
        let s = generate_snippet(text, &["longword"], 5).unwrap();
        // The snippet content (excluding tags/ellipsis) should be <= 5 chars
        // of the original text. Since "longword" is longer than the window,
        // we expect a partial word with highlight tags.
        assert!(s.contains("<mark>"));
    }

    #[test]
    fn snippet_preserves_original_case_in_output() {
        let text = "The Brown FOX jumped";
        let s = generate_snippet(text, &["fox"], 100).unwrap();
        assert!(s.contains("<mark>FOX</mark>"), "snippet was: {}", s);
    }

    #[test]
    fn snippet_with_overlapping_needle_substrings() {
        // "cat" and "catalog" — both should match the word "catalog" only
        // if it equals one of them exactly (whole-word matching).
        let text = "the catalog has a cat";
        let s = generate_snippet(text, &["cat", "catalog"], 100).unwrap();
        assert!(s.contains("<mark>catalog</mark>"));
        assert!(s.contains("<mark>cat</mark>"));
    }
}
