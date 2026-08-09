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

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
