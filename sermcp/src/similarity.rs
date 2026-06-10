//! Shared 3-gram Jaccard text similarity.
//!
//! One implementation for both consumers: the boot-detector's
//! `StageLearner` (which pre-computes fingerprint anchor grams and compares
//! many lines against them) and the connection learner's pairwise log
//! comparison. Both previously carried byte-identical copies.

/// Pre-compute a string's 3-gram hashes (one u64 per byte-window of 3).
pub fn trigram_hashes(s: &str) -> Vec<u64> {
    s.as_bytes()
        .windows(3)
        .map(|w| ((w[0] as u64) << 16) | ((w[1] as u64) << 8) | (w[2] as u64))
        .collect()
}

/// Jaccard similarity over pre-computed gram sets (0.0 when either side is
/// empty).
pub fn jaccard_similarity(a_grams: &[u64], b_grams: &[u64]) -> f64 {
    if a_grams.is_empty() || b_grams.is_empty() {
        return 0.0;
    }
    let a_set: std::collections::HashSet<u64> = a_grams.iter().copied().collect();
    let b_set: std::collections::HashSet<u64> = b_grams.iter().copied().collect();
    let intersection = a_set.intersection(&b_set).count();
    let union = a_set.len() + b_set.len() - intersection;
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

/// Convenience wrapper: Jaccard similarity of two strings' 3-gram sets.
pub fn jaccard_3gram(a: &str, b: &str) -> f64 {
    jaccard_similarity(&trigram_hashes(a), &trigram_hashes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known-value anchor: the exact trigram encodings for "abcd".
    /// Kills bit-level mutations (`<<`→`>>`, `|`→`^`) that relative/
    /// invariant assertions cannot see — flagged MISSED by cargo-mutants.
    #[test]
    fn trigram_hashes_known_values() {
        let hashes = trigram_hashes("abcd");
        assert_eq!(hashes.len(), 2);
        // (a<<16)|(b<<8)|c with a=0x61, b=0x62, c=0x63, d=0x64:
        assert_eq!(hashes[0], 0x0061_6263);
        assert_eq!(hashes[1], 0x0062_6364);
        // Empty/short inputs produce no windows.
        assert!(trigram_hashes("").is_empty());
        assert!(trigram_hashes("ab").is_empty());
    }

    /// The empty-side guard: jaccard against an empty gram set is 0.0
    /// (kills the `||`→`&&` arm mutation found by cargo-mutants).
    #[test]
    fn jaccard_empty_side_is_zero() {
        let grams = trigram_hashes("abcd");
        assert_eq!(jaccard_similarity(&grams, &[]), 0.0);
        assert_eq!(jaccard_similarity(&[], &grams), 0.0);
        assert_eq!(jaccard_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn identical_strings_score_one() {
        assert!((jaccard_3gram("U-Boot 2017.09", "U-Boot 2017.09") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn disjoint_strings_score_zero() {
        assert_eq!(jaccard_3gram("aaaa", "zzzz"), 0.0);
    }

    #[test]
    fn short_strings_score_zero_not_panic() {
        assert_eq!(jaccard_3gram("ab", "abc"), 0.0);
    }

    #[test]
    fn overlap_scores_between_zero_and_one() {
        let score = jaccard_3gram("kernel booting up", "kernel booting down");
        assert!(score > 0.0 && score < 1.0, "score={score}");
    }

    #[test]
    fn precomputed_grams_match_string_api() {
        let a = "Starting kernel ...";
        assert_eq!(
            jaccard_similarity(&trigram_hashes(a), &trigram_hashes(a)),
            1.0
        );
    }
}
