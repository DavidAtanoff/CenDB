//! Porter stemmer: reduces English words to their stem form.
//!
//! Implements the original Porter (1980) stemming algorithm — a deterministic
//! 5-step suffix-stripping algorithm that produces consistent word stems for
//! information retrieval. The algorithm uses the notion of a word's *measure*
//! `m`, defined as the number of `VC` repeats in the canonical form
//! `[C](VC){m}[V]` where `C` is a sequence of consonants and `V` is a sequence
//! of vowels.
//!
//! ## Steps
//!
//! * **1a**: plural normalization (`SSES`→`SS`, `IES`→`I`, `SS`→`SS`, `S`→``).
//! * **1b**: past tense / progressive (`EED`→`EE`, `ED`→``, `ING`→``) plus
//!   cleanup rules (`AT`→`ATE`, `BL`→`BLE`, `IZ`→`IZE`, double-consonant
//!   reduction, `cvc` repair).
//! * **1c**: `(*v*) Y`→`I`.
//! * **2**: suffix transformations (`ATIONAL`→`ATE`, `IZATION`→`IZE`, ...).
//! * **3**: suffix transformations (`ICATE`→`IC`, `ATIVE`→``, ...).
//! * **4**: suffix removal when `m > 1` (`AL`, `ANCE`, `EMENT`, `ION`, ...).
//! * **5a**: trailing `E` removal when `m > 1` or (`m = 1` and not `*o`).
//! * **5b**: trailing `L` reduction when `m > 1` and `*d` and `*L`.
//!
//! The algorithm operates on lowercase ASCII letters; non-ASCII characters
//! pass through unchanged.

/// Check if a character is one of the canonical vowels a, e, i, o, u.
fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Determine whether the character at index `i` in `word` is a consonant.
///
/// Letters other than `a, e, i, o, u` are consonants. The letter `y` is a
/// consonant when it is at the start of the word or follows a vowel, and a
/// vowel otherwise.
fn is_consonant_at(word: &[char], i: usize) -> bool {
    let c = word[i];
    if is_vowel(c) {
        return false;
    }
    if c == 'y' {
        if i == 0 {
            true
        } else {
            // y is consonant iff the previous letter is a vowel.
            !is_consonant_at(word, i - 1)
        }
    } else {
        // Any non-vowel ASCII letter (and any other char) is a consonant.
        true
    }
}

/// Compute the Porter *measure* `m` of a word: the number of `VC` repeats in
/// the canonical `[C](VC){m}[V]` form.
fn measure(word: &[char]) -> usize {
    let n = word.len();
    if n == 0 {
        return 0;
    }
    let mut m = 0;
    let mut i = 0;
    // Skip the optional leading consonant sequence [C].
    while i < n && is_consonant_at(word, i) {
        i += 1;
    }
    // Process VC pairs.
    while i < n {
        // Skip the vowel run V.
        while i < n && !is_consonant_at(word, i) {
            i += 1;
        }
        if i >= n {
            break;
        }
        // Skip the consonant run C — this completes one VC pair.
        while i < n && is_consonant_at(word, i) {
            i += 1;
        }
        m += 1;
    }
    m
}

/// `*v*` — true if the word contains at least one vowel.
fn contains_vowel(word: &[char]) -> bool {
    (0..word.len()).any(|i| !is_consonant_at(word, i))
}

/// `*d` — true if the word ends in a double consonant (same letter twice,
/// where that letter is a consonant in context).
fn ends_double_consonant(word: &[char]) -> bool {
    let n = word.len();
    if n < 2 {
        return false;
    }
    if word[n - 1] == word[n - 2] && is_consonant_at(word, n - 1) {
        return true;
    }
    false
}

/// `*o` — true if the word ends in `consonant-vowel-consonant` where the
/// final consonant is not `w`, `x`, or `y`.
fn ends_cvc(word: &[char]) -> bool {
    let n = word.len();
    if n < 3 {
        return false;
    }
    if !is_consonant_at(word, n - 3) {
        return false;
    }
    if is_consonant_at(word, n - 2) {
        return false;
    }
    if !is_consonant_at(word, n - 1) {
        return false;
    }
    let last = word[n - 1];
    if last == 'w' || last == 'x' || last == 'y' {
        return false;
    }
    true
}

/// True if `word` ends with `suffix`.
fn ends_with(word: &[char], suffix: &[char]) -> bool {
    let n = word.len();
    let s = suffix.len();
    if n < s {
        return false;
    }
    &word[n - s..] == suffix
}

/// True if the word's last character is one of `chars`.
fn ends_with_one_of(word: &[char], chars: &[char]) -> bool {
    match word.last() {
        Some(c) => chars.contains(c),
        None => false,
    }
}

/// Apply a `(m>0) SUFFIX -> REPLACEMENT` rule. Returns `true` if applied.
fn apply_rule_m_gt_0(w: &mut Vec<char>, suffix: &[char], replacement: &[char]) -> bool {
    if ends_with(w, suffix) {
        let stem_len = w.len() - suffix.len();
        let stem = &w[..stem_len];
        if measure(stem) > 0 {
            w.truncate(stem_len);
            w.extend_from_slice(replacement);
            return true;
        }
    }
    false
}

/// Step 1a: normalize plural suffixes.
fn step_1a(w: &mut Vec<char>) {
    if ends_with(w, &['s', 's', 'e', 's']) {
        w.truncate(w.len() - 2);
    } else if ends_with(w, &['i', 'e', 's']) {
        w.truncate(w.len() - 2);
    } else if ends_with(w, &['s', 's']) {
        // No change.
    } else if ends_with(w, &['s']) && w.len() > 1 {
        w.truncate(w.len() - 1);
    }
}

/// Step 1b: past tense / progressive, plus the cleanup rules.
fn step_1b(w: &mut Vec<char>) {
    let mut modified = false;
    if ends_with(w, &['e', 'e', 'd']) {
        // (m>0) EED -> EE
        if w.len() >= 3 {
            let stem = &w[..w.len() - 3];
            if measure(stem) > 0 {
                w.truncate(w.len() - 1); // remove just the 'd'
            }
        }
    } else if ends_with(w, &['e', 'd']) {
        // (*v*) ED ->
        if w.len() >= 2 {
            let stem = &w[..w.len() - 2];
            if contains_vowel(stem) {
                w.truncate(w.len() - 2);
                modified = true;
            }
        }
    } else if ends_with(w, &['i', 'n', 'g']) {
        // (*v*) ING ->
        if w.len() >= 3 {
            let stem = &w[..w.len() - 3];
            if contains_vowel(stem) {
                w.truncate(w.len() - 3);
                modified = true;
            }
        }
    }

    if modified {
        if ends_with(w, &['a', 't']) {
            w.push('e');
        } else if ends_with(w, &['b', 'l']) {
            w.push('e');
        } else if ends_with(w, &['i', 'z']) {
            w.push('e');
        } else if ends_double_consonant(w) && !ends_with_one_of(w, &['l', 's', 'z']) {
            // (*d and not (*L or *S or *Z)) -> single letter
            w.truncate(w.len() - 1);
        } else if measure(w) == 1 && ends_cvc(w) {
            // (m=1 and *o) -> E
            w.push('e');
        }
    }
}

/// Step 1c: `(*v*) Y` -> `I`.
fn step_1c(w: &mut Vec<char>) {
    if w.len() > 1 && w[w.len() - 1] == 'y' {
        let has_vowel = contains_vowel(&w[..w.len() - 1]);
        if has_vowel {
            let last = w.len() - 1;
            w[last] = 'i';
        }
    }
}

/// Step 2: `m>0` suffix transformations.
fn step_2(w: &mut Vec<char>) {
    // Rules ordered with longer / more specific suffixes first.
    let rules: &[(&[char], &[char])] = &[
        (&['a', 't', 'i', 'o', 'n', 'a', 'l'], &['a', 't', 'e']),
        (&['t', 'i', 'o', 'n', 'a', 'l'], &['t', 'i', 'o', 'n']),
        (&['e', 'n', 'c', 'i'], &['e', 'n', 'c', 'e']),
        (&['a', 'n', 'c', 'i'], &['a', 'n', 'c', 'e']),
        (&['i', 'z', 'e', 'r'], &['i', 'z', 'e']),
        (&['a', 'b', 'l', 'i'], &['a', 'b', 'l', 'e']),
        (&['a', 'l', 'l', 'i'], &['a', 'l']),
        (&['e', 'n', 't', 'l', 'i'], &['e', 'n', 't']),
        (&['e', 'l', 'i'], &['e']),
        (&['o', 'u', 's', 'l', 'i'], &['o', 'u', 's']),
        (&['i', 'z', 'a', 't', 'i', 'o', 'n'], &['i', 'z', 'e']),
        (&['a', 't', 'i', 'o', 'n'], &['a', 't', 'e']),
        (&['a', 't', 'o', 'r'], &['a', 't', 'e']),
        (&['a', 'l', 'i', 's', 'm'], &['a', 'l']),
        (&['i', 'v', 'e', 'n', 'e', 's', 's'], &['i', 'v', 'e']),
        (&['f', 'u', 'l', 'n', 'e', 's', 's'], &['f', 'u', 'l']),
        (&['o', 'u', 's', 'n', 'e', 's', 's'], &['o', 'u', 's']),
        (&['a', 'l', 'i', 't', 'i'], &['a', 'l']),
        (&['i', 'v', 'i', 't', 'i'], &['i', 'v', 'e']),
        (&['b', 'i', 'l', 'i', 't', 'i'], &['b', 'l', 'e']),
    ];
    for (suffix, replacement) in rules {
        if apply_rule_m_gt_0(w, suffix, replacement) {
            return;
        }
    }
    // (m>0) LOGI -> LOG
    if ends_with(w, &['l', 'o', 'g', 'i']) {
        let stem_len = w.len() - 4;
        let stem = &w[..stem_len];
        if measure(stem) > 0 {
            w.truncate(stem_len);
            w.extend_from_slice(&['l', 'o', 'g']);
        }
    }
}

/// Step 3: `m>0` suffix transformations.
fn step_3(w: &mut Vec<char>) {
    let rules: &[(&[char], &[char])] = &[
        (&['i', 'c', 'a', 't', 'e'], &['i', 'c']),
        (&['a', 't', 'i', 'v', 'e'], &[]),
        (&['a', 'l', 'i', 'z', 'e'], &['a', 'l']),
        (&['i', 'c', 'i', 't', 'i'], &['i', 'c']),
        (&['i', 'c', 'a', 'l'], &['i', 'c']),
        (&['f', 'u', 'l'], &[]),
        (&['n', 'e', 's', 's'], &[]),
    ];
    for (suffix, replacement) in rules {
        if apply_rule_m_gt_0(w, suffix, replacement) {
            return;
        }
    }
}

/// Step 4: remove suffixes when `m > 1`. `ION` is only removed when the stem
/// ends in `s` or `t`.
fn step_4(w: &mut Vec<char>) {
    let suffixes: &[&[char]] = &[
        &['a', 'l'],
        &['a', 'n', 'c', 'e'],
        &['e', 'n', 'c', 'e'],
        &['e', 'r'],
        &['i', 'c'],
        &['a', 'b', 'l', 'e'],
        &['i', 'b', 'l', 'e'],
        &['a', 'n', 't'],
        &['e', 'm', 'e', 'n', 't'],
        &['m', 'e', 'n', 't'],
        &['e', 'n', 't'],
        &['i', 'o', 'n'],
        &['o', 'u'],
        &['i', 's', 'm'],
        &['a', 't', 'e'],
        &['i', 't', 'i'],
        &['o', 'u', 's'],
        &['i', 'v', 'e'],
        &['i', 'z', 'e'],
    ];
    for suffix in suffixes {
        if ends_with(w, suffix) {
            let stem_len = w.len() - suffix.len();
            if stem_len == 0 {
                return;
            }
            let stem = &w[..stem_len];
            if measure(stem) > 1 {
                let is_ion = suffix.len() == 3
                    && suffix[0] == 'i'
                    && suffix[1] == 'o'
                    && suffix[2] == 'n';
                if is_ion {
                    let last = stem[stem.len() - 1];
                    if last == 's' || last == 't' {
                        w.truncate(stem_len);
                    }
                } else {
                    w.truncate(stem_len);
                }
            }
            return;
        }
    }
}

/// Step 5a: remove trailing `E` when `m > 1`, or when `m = 1` and not `*o`.
fn step_5a(w: &mut Vec<char>) {
    if w.is_empty() || w[w.len() - 1] != 'e' {
        return;
    }
    let stem = &w[..w.len() - 1];
    let m = measure(stem);
    if m > 1 {
        w.truncate(w.len() - 1);
    } else if m == 1 && !ends_cvc(stem) {
        w.truncate(w.len() - 1);
    }
}

/// Step 5b: reduce trailing double `L` when `m > 1` and `*d` and `*L`.
fn step_5b(w: &mut Vec<char>) {
    if w.is_empty() {
        return;
    }
    if measure(w) > 1 && ends_double_consonant(w) && w[w.len() - 1] == 'l' {
        w.truncate(w.len() - 1);
    }
}

/// Stem a single word using the Porter algorithm. Returns a lowercase stem.
///
/// Words of length 2 or shorter are returned unchanged (Porter convention).
/// Non-ASCII characters are preserved as-is; the algorithm only inspects
/// ASCII letters.
pub fn stem(word: &str) -> String {
    let lower = word.to_lowercase();
    if lower.len() <= 2 {
        return lower;
    }
    let mut w: Vec<char> = lower.chars().collect();
    step_1a(&mut w);
    step_1b(&mut w);
    step_1c(&mut w);
    step_2(&mut w);
    step_3(&mut w);
    step_4(&mut w);
    step_5a(&mut w);
    step_5b(&mut w);
    w.into_iter().collect()
}

/// Owned alias of [`stem`] — present for API symmetry with callers that
/// prefer an explicit `_owned` naming.
pub fn stem_owned(word: &str) -> String {
    stem(word)
}

/// Stem each whitespace/punctuation-delimited word in `text`, preserving all
/// non-word characters and their positions.
pub fn stem_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word_buf = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            word_buf.push(c);
        } else {
            if !word_buf.is_empty() {
                out.push_str(&stem(&word_buf));
                word_buf.clear();
            }
            out.push(c);
        }
    }
    if !word_buf.is_empty() {
        out.push_str(&stem(&word_buf));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_plurals() {
        assert_eq!(stem("cats"), "cat");
        assert_eq!(stem("ponies"), "poni");
        assert_eq!(stem("caresses"), "caress");
        assert_eq!(stem("cat"), "cat");
        assert_eq!(stem("dogs"), "dog");
    }

    #[test]
    fn stem_past_tense_and_progressive() {
        assert_eq!(stem("running"), "run");
        assert_eq!(stem("hopping"), "hop");
        assert_eq!(stem("plastered"), "plaster");
        assert_eq!(stem("motoring"), "motor");
        assert_eq!(stem("agreed"), "agre");
        assert_eq!(stem("feed"), "feed"); // m=0, EED unchanged
        assert_eq!(stem("bled"), "bled"); // no vowel in stem, ED unchanged
        assert_eq!(stem("sing"), "sing"); // no vowel in stem, ING unchanged
    }

    #[test]
    fn stem_y_to_i() {
        assert_eq!(stem("happily"), "happili");
        // "sky" — step 1c: stem "sk" has no vowel, so Y is not replaced.
        assert_eq!(stem("sky"), "sky");
    }

    #[test]
    fn step2_transformations() {
        assert_eq!(stem("relational"), "relat"); // ATIONAL -> ATE, then step 4 ATE removed (m>1)
        assert_eq!(stem("conditional"), "condit"); // TIONAL -> TION, then step 4 TION removed (preceded by t)
        assert_eq!(stem("rational"), "ration"); // TIONAL -> TION, m("ra")=1, then step 4 TION removed? stem "ra" m=1, not >1, so stays "ration"
        assert_eq!(stem("valenci"), "valenc"); // ENCI -> ENCE
        assert_eq!(stem("digitizer"), "digit"); // IZER -> IZE, then step 4 IZE removed
        assert_eq!(stem("conformabli"), "conform"); // ABLI -> ABLE, then step 4 ABLE removed
        assert_eq!(stem("radicalli"), "radic"); // ALLI -> AL, then step 4 AL removed
        assert_eq!(stem("differentli"), "differ"); // ENTLI -> ENT, then step 4 ENT removed
        assert_eq!(stem("vileli"), "vile"); // ELI -> E
        assert_eq!(stem("analogousli"), "analog"); // OUSLI -> OUS, then step 4 OUS removed
        assert_eq!(stem("vietnamization"), "vietnam"); // IZATION -> IZE, then step 4 IZE removed
        assert_eq!(stem("predication"), "predic"); // ATION -> ATE, then step 4 ATE removed
        assert_eq!(stem("operator"), "oper"); // ATOR -> ATE, then step 4 ATE removed
        assert_eq!(stem("feudalism"), "feudal"); // ALISM -> AL
        assert_eq!(stem("decisiveness"), "decis"); // IVENESS -> IVE, then step 4 IVE removed
        assert_eq!(stem("hopefulness"), "hope"); // FULNESS -> FUL, then step 4 FUL? not in list. stays "hopeful"? wait FUL is in step 3 not 4
        assert_eq!(stem("callousness"), "callous"); // OUSNESS -> OUS
        assert_eq!(stem("formaliti"), "formal"); // ALITI -> AL
        assert_eq!(stem("sensitiviti"), "sensit"); // IVITI -> IVE, then step 4 IVE removed
        assert_eq!(stem("sensibiliti"), "sensibl"); // BILITI -> BLE
    }

    #[test]
    fn step3_transformations() {
        assert_eq!(stem("triplicate"), "triplic"); // ICATE -> IC
        assert_eq!(stem("formative"), "form"); // ATIVE ->
        assert_eq!(stem("formalize"), "formal"); // ALIZE -> AL
        assert_eq!(stem("electriciti"), "electr"); // ICITI -> IC, then step 4 IC removed
        assert_eq!(stem("electrical"), "electr"); // ICAL -> IC, then step 4 IC removed
        assert_eq!(stem("hopeful"), "hope"); // FUL -> (step 3)
        assert_eq!(stem("goodness"), "good"); // NESS -> (step 3)
    }

    #[test]
    fn step4_removals() {
        assert_eq!(stem("revival"), "reviv"); // AL removed
        assert_eq!(stem("allowance"), "allow"); // ANCE removed
        assert_eq!(stem("inference"), "infer"); // ENCE removed
        assert_eq!(stem("airliner"), "airlin"); // ER removed
        assert_eq!(stem("gyroscopic"), "gyroscop"); // IC removed
        assert_eq!(stem("adjustable"), "adjust"); // ABLE removed
        assert_eq!(stem("defensible"), "defens"); // IBLE removed
        assert_eq!(stem("irritant"), "irrit"); // ANT removed
        assert_eq!(stem("replacement"), "replac"); // EMENT removed
        assert_eq!(stem("adjustment"), "adjust"); // MENT removed
        assert_eq!(stem("dependent"), "depend"); // ENT removed
        assert_eq!(stem("adoption"), "adopt"); // ION removed (preceded by t)
        assert_eq!(stem("homologou"), "homolog"); // OU removed
        assert_eq!(stem("communism"), "commun"); // ISM removed
        assert_eq!(stem("activate"), "activ"); // ATE removed
        assert_eq!(stem("angulariti"), "angular"); // ITI -> already step 2 ALITI? no. ITI is in step 4. angulariti -> angular (ITI removed, m>1)
        assert_eq!(stem("homologous"), "homolog"); // OUS removed
        assert_eq!(stem("effective"), "effect"); // IVE removed
        assert_eq!(stem("bowdlerize"), "bowdler"); // IZE removed
    }

    #[test]
    fn step5_trailing_e_and_l() {
        assert_eq!(stem("probate"), "probat"); // (m>1) E -> removed? "probat" m=2, so step5a removes E → "probat"
        assert_eq!(stem("rate"), "rate"); // (m=1 and *o) E -> kept. "rat" m=1, ends_cvc("rat")=true, so E kept
        assert_eq!(stem("cease"), "ceas"); // (m>1) E -> removed. "ceas" m=1? c-e-a-s → [C=c][V=ea][C=s] → m=1. Not >1. m=1 and not *o? ends_cvc("ceas")? c-e-a-s, last 3: e,a,s. 'e' consonant? no (vowel). So ends_cvc=false. So m=1 and not *o → remove E → "ceas"
        assert_eq!(stem("controll"), "control"); // (m>1 and *d and *L) -> single letter. "controll" m=3, ends double l → remove one → "control"
        assert_eq!(stem("roll"), "roll"); // (m>1 and *d and *L). "roll" m=1, not >1, so no removal. stays "roll"
    }

    #[test]
    fn stem_short_words_unchanged() {
        assert_eq!(stem("a"), "a");
        assert_eq!(stem("is"), "is");
        assert_eq!(stem("be"), "be");
    }

    #[test]
    fn stem_preserves_case_insensitive() {
        assert_eq!(stem("Running"), "run");
        assert_eq!(stem("CATS"), "cat");
        assert_eq!(stem("HappILY"), "happili");
    }

    #[test]
    fn stem_text_basic() {
        let s = stem_text("The cats are running happily");
        // "the" → "the" (len 3, but no rule applies → "the")
        // "cats" → "cat"
        // "are" → "are" (no rule)
        // "running" → "run"
        // "happily" → "happili"
        assert!(s.contains("cat"));
        assert!(s.contains("run"));
        assert!(s.contains("happili"));
    }

    #[test]
    fn stem_text_preserves_punctuation() {
        let s = stem_text("cats, dogs; running!");
        assert!(s.contains("cat,"));
        assert!(s.contains("dog;"));
        assert!(s.contains("run!"));
    }

    #[test]
    fn stem_text_handles_empty() {
        assert_eq!(stem_text(""), "");
        assert_eq!(stem_text("   "), "   ");
    }

    #[test]
    fn stem_owned_matches_stem() {
        assert_eq!(stem_owned("running"), stem("running"));
        assert_eq!(stem_owned("happily"), stem("happily"));
    }

    #[test]
    fn stem_non_ascii_passthrough() {
        // Non-ASCII letters pass through; length check uses char count.
        // "café" has 4 chars so the algorithm runs, but 'é' is treated as
        // a consonant (non-vowel ASCII). The result is well-defined but not
        // particularly meaningful; just ensure it doesn't panic.
        let _ = stem("café");
        let _ = stem("naïve");
    }

    #[test]
    fn stem_known_corpus() {
        // A handful of well-known Porter stemmer results.
        assert_eq!(stem("generalization"), "gener");
        assert_eq!(stem("osculations"), "oscul");
        assert_eq!(stem("obligations"), "oblig");
        assert_eq!(stem("dependent"), "depend");
        assert_eq!(stem("believing"), "believ");
    }
}
