//! The `<attached_context>` block the terminal appends to user messages.
//!
//! Everything machine-added to a prompt — skills, the working directory,
//! the active mode's instruction — rides in one block built by
//! [`attach_context_tag`]. The model reads it; the transcript must not, so
//! [`strip_attached_context`] keys on this module's marker rather than on the
//! wording inside it.

pub const ATTACHED_CONTEXT_OPEN: &str = "<attached_context>";

const ATTACHED_CONTEXT_FRAMING: &str = "This block was added by the terminal at send time. It is \
     context about where and how you are running, not part of the user's request.";

/// Wrap any machine-added body in the `<attached_context>` block.
pub fn attach_context_tag(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    Some(format!(
        "{ATTACHED_CONTEXT_OPEN}\n{ATTACHED_CONTEXT_FRAMING}\n\n{body}\n</attached_context>"
    ))
}

/// Cut a stored user message at the `<attached_context>` marker.
///
/// The block is always the *last* thing appended to a user message, and it
/// carries everything the terminal added at send time — skills included, as
/// a nested `<attached_skills>` block — so one cut removes it all. The cut
/// keys on the marker, immune to wording drift, the way a text-exact suffix
/// match would not be.
pub fn strip_attached_context(text: &str) -> &str {
    let cut = text.find(ATTACHED_CONTEXT_OPEN).unwrap_or(text.len());
    text[..cut].trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, format_inline_skills};

    fn skill(name: &str, instructions: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: format!("{name} description"),
            instructions: instructions.to_owned(),
        }
    }

    #[test]
    fn attached_context_wraps_an_opaque_body() {
        let block = attach_context_tag("/tmp/project\n\nPlan mode is on").unwrap();

        assert!(block.starts_with(ATTACHED_CONTEXT_OPEN));
        assert!(block.contains("\n\n/tmp/project\n\nPlan mode is on\n"));
        assert!(block.trim_end().ends_with("</attached_context>"));
    }

    #[test]
    fn attached_context_omits_empty_bodies() {
        assert_eq!(attach_context_tag(""), None);
        assert_eq!(attach_context_tag("   \n  "), None);
    }

    /// The whole point of the marker: stripping must never depend on the
    /// wording inside the block, so changed instructions cannot leave old
    /// sessions un-strippable.
    #[test]
    fn attached_context_strips_at_the_marker_only() {
        let block = attach_context_tag("Plan mode is on").unwrap();
        let stored = format!("ship it\n\n{block}");

        assert_eq!(strip_attached_context(&stored), "ship it");
    }

    /// Skills ride inside the context block, so the single cut drops both.
    #[test]
    fn attached_context_strips_skills_nested_inside_it() {
        let skills = format_inline_skills(&[skill("deploy", "Run the checklist.")]).unwrap();
        let stored = format!(
            "ship it #deploy\n\n{}",
            attach_context_tag(&skills).unwrap()
        );

        assert_eq!(strip_attached_context(&stored), "ship it #deploy");
    }

    #[test]
    fn attached_context_leaves_plain_messages_untouched() {
        assert_eq!(strip_attached_context("hello world"), "hello world");
        assert_eq!(strip_attached_context("hello world\n\n"), "hello world");
    }
}
