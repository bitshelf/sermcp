//! Marker generation — modeled on labgrid util/marker.py
//!
//! Generates a 10-char random marker, excluding R/I/D to avoid collisions
//! with log keywords such as ERROR/FAIL/INFO/DEBUG.

// rand 0.10 renamed the old RngCore convenience trait to RngExt:
// `use rand::Rng;` now imports the core trait (no random_range()).
use rand::RngExt;

/// Available character pool: A-Z excluding R, I, D
const MARKER_POOL: &[u8] = b"ABCEFGHJKLMNOPQSTUVWXYZ";

/// Generate a 10-char random uppercase-letter marker
pub fn gen_marker() -> String {
    let mut rng = rand::rng();
    (0..10)
        .map(|_| {
            let idx = rng.random_range(0..MARKER_POOL.len());
            MARKER_POOL[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn marker_length() {
        assert_eq!(gen_marker().len(), 10);
    }

    #[test]
    fn marker_chars() {
        let m = gen_marker();
        assert!(m.chars().all(|c| c.is_ascii_uppercase()));
        assert!(!m.contains('R'));
        assert!(!m.contains('I'));
        assert!(!m.contains('D'));
    }

    #[test]
    fn marker_unique() {
        let a = gen_marker();
        let b = gen_marker();
        assert_ne!(a, b);
    }

    #[test]
    fn test_marker_many_unique() {
        let mut markers = HashSet::new();
        for _ in 0..1000 {
            let m = gen_marker();
            assert!(markers.insert(m), "Duplicate marker generated");
        }
        assert_eq!(markers.len(), 1000);
    }

    #[test]
    fn test_marker_all_from_pool() {
        for _ in 0..100 {
            let m = gen_marker();
            for c in m.chars() {
                assert!(MARKER_POOL.contains(&(c as u8)), "Char {} not in pool", c);
            }
        }
    }

    #[test]
    fn test_marker_pool_size() {
        // A-Z = 26, minus R,I,D = 23
        assert_eq!(MARKER_POOL.len(), 23);
    }

    #[test]
    fn test_marker_excluded_chars() {
        // Verify R, I, D are not in pool
        assert!(!MARKER_POOL.contains(&b'R'));
        assert!(!MARKER_POOL.contains(&b'I'));
        assert!(!MARKER_POOL.contains(&b'D'));
    }
}
