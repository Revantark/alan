//! Display-only helpers shared by the event stream and transcript restoration.
//!
//! Live tool output reaches the UI via `ToolCallFinished`, which already
//! carries a tail; restored transcripts must reproduce that same tail, so
//! both paths share the helper and its line budget here.

/// How many trailing output lines a tool call shows in the transcript.
pub const TOOL_OUTPUT_TAIL_LINES: usize = 5;

/// Keep only the last `count` lines of `output`.
pub fn tail_lines(output: &str, count: usize) -> String {
    let mut lines: Vec<_> = output.lines().rev().take(count).collect();
    lines.reverse();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_n_lines_in_order() {
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
    }

    #[test]
    fn tail_is_unchanged_when_output_fits() {
        let output = "l1\nl2\nl3";
        assert_eq!(tail_lines(output, TOOL_OUTPUT_TAIL_LINES), output);
    }
}
