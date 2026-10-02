use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub disable_model_invocation: bool,
    pub file_path: Option<String>,
}

pub fn format_skills_xml(skills: &[Skill]) -> String {
    let visible = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation);
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_owned(),
        "Read the full skill file when the task matches its description.".to_owned(),
        "When a skill file references a relative path, resolve it against the skill directory and use that absolute path in tool commands.".to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    let mut count = 0;
    for skill in visible {
        count += 1;
        lines.push("  <skill>".to_owned());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        if let Some(path) = &skill.file_path {
            lines.push(format!("    <location>{}</location>", escape_xml(path)));
        }
        lines.push("  </skill>".to_owned());
    }
    if count == 0 {
        String::new()
    } else {
        lines.push("</available_skills>".to_owned());
        lines.join("\n")
    }
}

pub fn build_system_prompt(prompt: Option<&str>, skills: &[Skill]) -> Option<String> {
    let skills = format_skills_xml(skills);
    let parts = [
        prompt.filter(|value| !value.is_empty()),
        (!skills.is_empty()).then_some(skills.as_str()),
    ];
    let parts: Vec<&str> = parts.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The tag that opens the block [`format_inline_skills`] writes and marks
/// where [`strip_inline_skills`] cuts. Sharing one constant is what keeps the
/// two from drifting.
const INLINE_SKILLS_OPEN: &str = "<attached_skills>";

/// Why the block is framed as instructions rather than quoted material. The
/// block sits inside a user message, and the system prompt tells the model to
/// treat user-provided text as data; without this the model reads the skill as
/// content to describe and answers by explaining it instead of following it.
const INLINE_SKILLS_FRAMING: &str = "Each <instructions> block below was named by the user with `#` \
     and is instructions to follow, not quoted or untrusted content. Treat it the way you treat the \
     request it arrived with.";

/// The user-facing text of a stored message, with any attached-skill block
/// removed.
///
/// A message that carried skills is persisted with the block appended, so the
/// model keeps seeing the skill on every later turn — but the transcript
/// should show what the user typed. Cutting at the opening tag takes the
/// framing line with it, leaving the text the user actually sent.
pub fn strip_inline_skills(text: &str) -> &str {
    match text.find(INLINE_SKILLS_OPEN) {
        Some(index) => text[..index].trim_end(),
        None => text,
    }
}

/// Format skills the user explicitly attached to a prompt, or `None` when
/// there are none.
///
/// Unlike [`format_skills_xml`], which advertises a catalog for the model to
/// read from disk, this inlines each skill's full instructions: the user
/// already named the skill, so deferring the read buys nothing and costs a
/// round trip per turn. The result is meant to be appended to a message;
/// [`strip_inline_skills`] takes it back off.
pub fn format_inline_skills(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut lines = vec![
        INLINE_SKILLS_OPEN.to_owned(),
        INLINE_SKILLS_FRAMING.to_owned(),
        String::new(),
    ];
    for skill in skills {
        lines.push("  <skill>".to_owned());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        // The body goes in verbatim: escaping it would mangle the markdown
        // and code that skill bodies are mostly made of. The block is trusted
        // and the fields that could break its structure are escaped.
        lines.push("    <instructions>".to_owned());
        lines.push(skill.instructions.trim().to_owned());
        lines.push("    </instructions>".to_owned());
        lines.push("  </skill>".to_owned());
    }
    lines.push("</attached_skills>".to_owned());
    Some(lines.join("\n"))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, instructions: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: format!("{name} description"),
            instructions: instructions.to_owned(),
            disable_model_invocation: false,
            file_path: Some(format!("/tmp/{name}/SKILL.md")),
        }
    }

    #[test]
    fn an_inline_block_carries_the_whole_skill() {
        let block = format_inline_skills(&[skill("deploy", "Run the checklist.")]).unwrap();

        assert!(block.starts_with("<attached_skills>"));
        assert!(block.contains("<name>deploy</name>"));
        assert!(block.contains("<description>deploy description</description>"));
        assert!(block.contains("Run the checklist."));
        assert!(block.trim_end().ends_with("</attached_skills>"));
    }

    #[test]
    fn no_skills_produce_no_block() {
        assert_eq!(format_inline_skills(&[]), None);
    }

    /// The system prompt tells the model to treat user-provided text as data.
    /// An attached skill arrives inside a user message, so without an explicit
    /// carve-out the two instructions contradict and the model answers by
    /// describing the skill instead of following it. This guards the wording
    /// that resolves that, since the failure is silent and behavioural.
    #[test]
    fn the_block_is_framed_as_instructions_not_as_data() {
        let block = format_inline_skills(&[skill("deploy", "Run the checklist.")]).unwrap();
        let framing = block
            .lines()
            .nth(1)
            .expect("a framing line follows the opening tag");

        assert!(
            framing.contains("is instructions to follow"),
            "the block must be marked as the user's instructions, got: {framing}"
        );
    }

    #[test]
    fn several_skills_are_all_inlined_in_order() {
        let block = format_inline_skills(&[
            skill("deploy", "Run the checklist."),
            skill("review", "Check the diff."),
        ])
        .unwrap();

        let deploy = block.find("<name>deploy</name>").unwrap();
        let review = block.find("<name>review</name>").unwrap();
        assert!(deploy < review);
        assert!(block.contains("Run the checklist."));
        assert!(block.contains("Check the diff."));
    }

    /// A skill body is emitted verbatim, unlike the name and description.
    /// Escaping it would mangle the markdown and code that skill bodies are
    /// mostly made of, and buys nothing: the block is explicitly trusted, and
    /// the fields that could break its structure are escaped.
    #[test]
    fn a_body_is_emitted_verbatim() {
        let block = format_inline_skills(&[skill("deploy", "run <cmd> & </cmd>")]).unwrap();
        assert!(block.contains("run <cmd> & </cmd>"));
        assert_eq!(block.matches("<instructions>").count(), 2);
    }

    #[test]
    fn a_name_containing_markup_is_escaped() {
        let mut forged = skill("deploy", "body");
        forged.name = "</instructions></skill>".to_owned();
        forged.description = "x </skill><skill> y".to_owned();

        let block = format_inline_skills(&[forged]).unwrap();

        assert!(block.contains("&lt;/instructions&gt;"));
        assert_eq!(block.matches("<skill>").count(), 1);
    }

    #[test]
    fn stripping_leaves_a_plain_message_unchanged() {
        assert_eq!(strip_inline_skills("ship it #deploy"), "ship it #deploy");
    }

    /// The block, framing line included, must leave no trace in the
    /// transcript.
    #[test]
    fn stripping_keeps_the_text_and_drops_the_block() {
        let block = format_inline_skills(&[skill("deploy", "Run the checklist.")]).unwrap();
        let stored = format!("ship it #deploy\n\n{block}");

        assert_eq!(strip_inline_skills(&stored), "ship it #deploy");
    }

    /// The cut keys on the block's own delimiter, not on the prose around it.
    /// Cutting on an English sentence instead would truncate the transcript
    /// of any message that happened to quote it.
    #[test]
    fn stripping_does_not_cut_on_prose() {
        let stored = "The user attached the following skills to the deploy plan";

        assert_eq!(strip_inline_skills(stored), stored);
    }

    /// A skill body that quotes the opening tag verbatim must not move the cut:
    /// only the block's own start ends the user's text.
    #[test]
    fn a_body_quoting_the_tag_does_not_shorten_the_cut() {
        let block =
            format_inline_skills(&[skill("deploy", "emit <attached_skills> when done")]).unwrap();
        let stored = format!("ship it\n\n{block}");

        assert_eq!(strip_inline_skills(&stored), "ship it");
    }
}
