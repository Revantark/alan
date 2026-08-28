//! Ranking candidates against a typed pattern.
//!
//! A pattern is a path fragment. Its `/`-separated parts must appear in the
//! candidate in order, but may skip whole directories, so `crates/main.rs`
//! finds `crates/alan/src/main.rs`. Matching ignores ASCII case.

/// How good a match is. Lower sorts first, and the field order is the
/// tie-break order, so rearranging either reorders every completion popup.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
struct Rank {
    kind: Kind,
    /// Length of the candidate.
    length: usize,
    /// Byte offset of the match, to settle equal-length candidates.
    at: usize,
}

/// Where in the candidate the match landed.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum Kind {
    /// The candidate is the pattern.
    Exact,
    /// The candidate begins with it.
    Prefix,
    /// The candidate's own name begins with it: `main` in `src/main.rs`.
    Name,
    /// Anywhere else.
    Contains,
}

/// Indexes into `candidates` of everything matching `pattern`, best first.
///
/// Every function here takes the pattern before the text it is matched against.
pub fn rank_all<S: AsRef<str>>(pattern: &str, candidates: &[S]) -> Vec<usize> {
    if pattern.is_empty() {
        return (0..candidates.len()).collect();
    }
    let mut matched: Vec<(usize, Rank)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| Some((index, rank(pattern, candidate.as_ref())?)))
        .collect();
    matched.sort_by_key(|&(_, rank)| rank);
    matched.into_iter().map(|(index, _)| index).collect()
}

fn rank(pattern: &str, candidate: &str) -> Option<Rank> {
    let at = match_start(pattern, candidate)?;

    // Where the candidate's own name starts, a directory's trailing `/` aside.
    let name = candidate
        .trim_end_matches('/')
        .rfind('/')
        .map_or(0, |slash| slash + 1);
    // The last part is the name being typed; the parts before it only say where
    // to look. `src/s` means a name starting with `s`, not any path under `src`
    // that happens to contain one.
    let typed = pattern.rsplit('/').find(|part| !part.is_empty())?;

    // Equal lengths do not imply equal text, since a pattern may skip segments.
    let kind = if candidate.eq_ignore_ascii_case(pattern) {
        Kind::Exact
    } else if at == 0 {
        Kind::Prefix
    } else if starts_ignoring_case(&candidate[name..], typed) {
        Kind::Name
    } else {
        Kind::Contains
    };
    Some(Rank {
        kind,
        length: candidate.len(),
        at,
    })
}

fn starts_ignoring_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Byte offset in `candidate` where the first of `pattern`'s parts matches,
/// once every part has been found in order. `None` if any part is missing.
fn match_start(pattern: &str, candidate: &str) -> Option<usize> {
    let mut start = None;
    let mut from = 0;
    for part in pattern.split('/').filter(|part| !part.is_empty()) {
        let at = find_from(part, candidate, from)?;
        start.get_or_insert(at);
        from = at + part.len();
    }
    start
}

/// Byte offset of `part` in `candidate` at or after `from`, ignoring ASCII
/// case. Byte-wise because lowercasing both sides would allocate for every
/// candidate on every keystroke, and slicing bytes cannot land off a `char`
/// boundary.
fn find_from(part: &str, candidate: &str, from: usize) -> Option<usize> {
    candidate.as_bytes()[from..]
        .windows(part.len())
        .position(|window| window.eq_ignore_ascii_case(part.as_bytes()))
        .map(|at| from + at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked<'a>(pattern: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        rank_all(pattern, candidates)
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
    fn ranks_exact_then_prefix_then_name_then_anywhere() {
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

    /// All three match on the name, so only the tie-breaks separate them.
    #[test]
    fn tie_breaks_on_length_then_position() {
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

    /// A path typed from memory skips the segments in between.
    #[test]
    fn pattern_segments_may_skip_directories() {
        let candidates = ["crates/alan/src/main.rs", "crates/agent/src/lib.rs"];

        assert_eq!(
            ranked("crates/main.rs", &candidates),
            ["crates/alan/src/main.rs"]
        );
        assert_eq!(
            ranked("alan/main", &candidates),
            ["crates/alan/src/main.rs"]
        );
    }

    /// Skipping segments is not the same as ignoring their order.
    #[test]
    fn pattern_segments_must_appear_in_order() {
        let candidates = ["crates/alan/src/main.rs"];

        assert_eq!(ranked("main/crates", &candidates), Vec::<&str>::new());
    }

    /// The last part is the name being typed; the parts before it only locate
    /// it. Every `.rs` file ends in `s`, which must not count as a name match.
    #[test]
    fn the_last_pattern_part_ranks_against_the_name() {
        let candidates = ["crates/llm/src/lib.rs", "crates/agent/src/skill.rs"];

        assert_eq!(
            ranked("src/s", &candidates),
            ["crates/agent/src/skill.rs", "crates/llm/src/lib.rs"]
        );
    }

    /// When several names match equally well the shortest path wins, rather
    /// than whichever has the fewest directories before the first part.
    #[test]
    fn equally_good_names_are_ordered_by_path_length() {
        let candidates = [
            "crates/llm/src/apis/chat_completions/sse.rs",
            "crates/alan/src/views/selection.rs",
            "crates/agent/src/skill.rs",
        ];

        assert_eq!(
            ranked("src/s", &candidates),
            [
                "crates/agent/src/skill.rs",
                "crates/alan/src/views/selection.rs",
                "crates/llm/src/apis/chat_completions/sse.rs",
            ]
        );
    }

    /// The name is what was typed, not the directory it happens to repeat in.
    #[test]
    fn a_name_match_outranks_the_same_text_in_the_directory_path() {
        let candidates = ["crates/tools/src/fs.rs", "crates/tools/src/tool.rs"];

        assert_eq!(
            ranked("tool", &candidates),
            ["crates/tools/src/tool.rs", "crates/tools/src/fs.rs"]
        );
    }

    /// A directory's own name is its last segment, trailing slash aside.
    #[test]
    fn a_directory_matches_on_its_own_name() {
        let candidates = ["crates/tools/", "crates/tools/src/args.rs"];

        assert_eq!(
            ranked("tools", &candidates),
            ["crates/tools/", "crates/tools/src/args.rs"]
        );
    }
}
