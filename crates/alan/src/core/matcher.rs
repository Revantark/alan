//! Pattern matching for completion candidates.

/// Quality of a match. The derived ordering is the ranking, so reordering
/// these variants reorders every completion popup.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum Kind {
    Exact,
    Prefix,
    Segment,
    Contains,
}

/// Indexes into `candidates` of everything matching `pattern`, best first.
pub fn match_all<S: AsRef<str>>(pattern: &str, candidates: &[S]) -> Vec<usize> {
    if pattern.is_empty() {
        return (0..candidates.len()).collect();
    }
    let mut matched: Vec<(usize, (Kind, usize, usize))> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| Some((index, rank(pattern, candidate.as_ref())?)))
        .collect();
    matched.sort_by_key(|&(_, rank)| rank);
    matched.into_iter().map(|(index, _)| index).collect()
}

/// Smaller is better in every field, so the tuple sorts best first.
fn rank(pattern: &str, candidate: &str) -> Option<(Kind, usize, usize)> {
    let at = find(candidate, pattern)?;
    // An exact match also starts at zero, so it has to be tested first.
    let kind = if candidate.len() == pattern.len() {
        Kind::Exact
    } else if at == 0 {
        Kind::Prefix
    } else if at == segment_start(candidate) {
        Kind::Segment
    } else {
        Kind::Contains
    };
    Some((kind, at, candidate.len()))
}

/// Byte-wise search is safe for UTF-8: an ASCII needle cannot match inside a
/// multi-byte sequence, whose bytes are all `>= 0x80`.
fn find(haystack: &str, needle: &str) -> Option<usize> {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if needle.is_empty() {
        // `windows(0)` panics.
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn segment_start(candidate: &str) -> usize {
    candidate.rfind('/').map_or(0, |slash| slash + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked<'a>(pattern: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        match_all(pattern, candidates)
            .into_iter()
            .map(|index| candidates[index])
            .collect()
    }

    #[test]
    fn empty_pattern_keeps_every_candidate_in_order() {
        assert_eq!(ranked("", &["b", "a", "c"]), ["b", "a", "c"]);
    }

    #[test]
    fn drops_candidates_that_do_not_contain_the_pattern() {
        assert_eq!(ranked("zz", &["alpha", "beta"]), Vec::<&str>::new());
    }

    #[test]
    fn ranks_exact_then_prefix_then_segment_then_substring() {
        let candidates = ["x/help", "helper", "help", "unhelpful"];
        assert_eq!(
            ranked("help", &candidates),
            ["help", "helper", "x/help", "unhelpful"]
        );
    }

    /// Guards the order the [`Kind`] variants are declared in.
    #[test]
    fn match_kind_outranks_any_tie_break() {
        let long_segment = format!("nested/{}", "a".repeat(300));
        let candidates = ["ba", long_segment.as_str()];
        assert_eq!(ranked("a", &candidates), [long_segment.as_str(), "ba"]);
    }

    /// All three are segment matches, so only the tie-breaks separate them.
    #[test]
    fn tie_breaks_on_position_then_length() {
        let candidates = ["crates/x/main.rs", "b/main_helper.rs", "a/main.rs"];
        assert_eq!(
            ranked("main", &candidates),
            ["a/main.rs", "b/main_helper.rs", "crates/x/main.rs"]
        );
    }

    #[test]
    fn ignores_ascii_case() {
        assert_eq!(ranked("HeLp", &["/help"]), ["/help"]);
    }
}
