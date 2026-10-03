//! Skill discovery and `#name` resolution.
//!
//! A skill is a directory holding a `SKILL.md`: YAML frontmatter for the
//! header, then the instructions themselves. The directory name is
//! authoritative and is what a `#name` token resolves against; the
//! frontmatter `name:` is not read at all.
//!
//! Roots are scanned project first, so `<cwd>/.alan/skills` shadows
//! `<data dir>/skills`, which in turn shadows the cross-tool
//! `<home>/.agents/skills`. The whole catalog is loaded once at startup.
//!
//! Only the frontmatter `description` is interpreted, and it is parsed by hand
//! rather than with a YAML dependency: the field is either an inline value or
//! a block scalar whose indented lines follow. See [`description_of`].

use agent::Skill;
use std::path::{Path, PathBuf};

/// Maximum characters of instructions kept per skill. Past this the skill is
/// truncated, so one runaway file cannot dominate the context window of every
/// prompt it is attached to.
const MAX_INSTRUCTIONS: usize = 8_000;

/// Maximum characters kept for a description. Descriptions are popup text, not
/// instructions, so a long one is a sign of a malformed file.
const MAX_DESCRIPTION: usize = 1_024;

/// Directory holding skills inside a root.
const SKILLS_DIR: &str = "skills";

/// Home-relative directory holding skills shared between agent tools.
const SHARED_DIR: &str = ".agents";

/// The only frontmatter key Alan reads. It is the text the user chooses the
/// skill from, so a skill without one is not usable.
const DESCRIPTION_KEY: &str = "description";

/// The project-local skills root for `cwd`: `<cwd>/.alan/skills`.
pub fn project_root(cwd: &Path) -> PathBuf {
    cwd.join(".alan").join(SKILLS_DIR)
}

/// The personal skills root: `<data dir>/skills`.
pub fn personal_root(data_dir: &Path) -> PathBuf {
    data_dir.join(SKILLS_DIR)
}

/// The cross-tool skills root under the home directory: `<home>/.agents/skills`.
///
/// This is the shared convention other agent tools use for skills they all read
/// from one place, so a skill written once here is not locked to Alan. It is
/// scanned after [`personal_root`], which keeps skills written for Alan itself
/// authoritative over the shared copy.
pub fn shared_root(home: &Path) -> PathBuf {
    home.join(SHARED_DIR).join(SKILLS_DIR)
}

/// Load every skill under `roots`, in order, keeping the first occurrence of
/// each name.
///
/// A missing root yields nothing and a broken skill is logged and skipped, so
/// one bad file does not cost the user the whole catalog.
pub fn load_all(roots: &[PathBuf]) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    for root in roots {
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(root) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .map(|e| e.path())
                .collect(),
            Err(error) => {
                // A missing directory is the common case, not a problem worth
                // a warning on every launch.
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(root = %root.display(), %error, "failed to read skills root");
                }
                continue;
            }
        };
        // `read_dir` order is filesystem-dependent; sort so the popup and the
        // attached-skill order are stable across runs.
        paths.sort();

        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if seen.iter().any(|seen| seen == name) {
                tracing::debug!(name, path = %path.display(), "skill name already loaded, skipping");
                continue;
            }
            match load_one(&path, name) {
                Ok(skill) => {
                    seen.push(name.to_owned());
                    skills.push(skill);
                }
                Err(error) => {
                    tracing::warn!(name, path = %path.display(), %error, "skipping skill")
                }
            }
        }
    }

    skills
}

/// Load one `<dir>/SKILL.md`.
///
/// A skill with no usable description cannot be chosen from the popup, so it
/// is rejected rather than attached blindly.
fn load_one(dir: &Path, name: &str) -> Result<Skill, String> {
    let path = dir.join("SKILL.md");
    let raw = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let description = description_of(&raw)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing or empty {DESCRIPTION_KEY}"))?;

    Ok(Skill {
        name: name.to_owned(),
        description: truncate(&collapse_whitespace(&description), MAX_DESCRIPTION),
        instructions: truncate(body_of(&raw).trim(), MAX_INSTRUCTIONS),
        // Alan attaches a skill only when the user types `#name`, and never
        // registers the catalog with the agent, so nothing reads this flag.
        disable_model_invocation: false,
        file_path: Some(path.to_string_lossy().into_owned()),
    })
}

/// Resolve the `#name` tokens in `text` against `catalog`.
///
/// Returns the matching skills in first-mention order, without duplicates. A
/// token naming a skill that is not in the catalog resolves to nothing and is
/// left in the text: it may be prose, and the model is better placed than a
/// silent editor to decide what a stray `#foo` means.
pub fn resolve_skills<'a>(catalog: &'a [Skill], text: &str) -> Vec<&'a Skill> {
    let mut resolved: Vec<&Skill> = Vec::new();

    for token in tokens(text) {
        let Some(skill) = catalog.iter().find(|skill| skill.name == token) else {
            tracing::debug!(token, "no skill matches token");
            continue;
        };
        if !resolved.iter().any(|seen| seen.name == skill.name) {
            resolved.push(skill);
        }
    }

    resolved
}

/// Every `#name` token in `text`, in order and with repeats.
///
/// A token runs from the `#` to the next whitespace, so `(#deploy)` and
/// `use:#deploy,` both name `deploy`.
fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace().filter_map(token_name)
}

/// The skill name a whitespace-delimited `word` names, if any.
///
/// This is the single definition of what a `#name` token is: the resolver
/// matches against it and the editor highlights by it, so a token the popup
/// offers cannot fail to attach, and one it does not offer cannot attach by
/// accident.
pub(crate) fn token_name(word: &str) -> Option<&str> {
    let (_, rest) = word.split_once('#')?;
    let name = rest.trim_matches(|c: char| c.is_ascii_punctuation());
    (!name.is_empty()).then_some(name)
}

/// The `description` in a document's frontmatter, if it has one.
///
/// The value is either inline (`description: Use when shipping`) or a YAML
/// block scalar (`description: >`) whose text sits on the indented lines
/// below. Any other key is skipped, along with its indented children, so an
/// unmodelled structure cannot be mistaken for the description's body.
fn description_of(raw: &str) -> Option<String> {
    let (frontmatter, _) = frontmatter_and_body(raw)?;

    let mut description: Option<String> = None;
    // Lines of the block scalar being read, and whether the `description` key
    // has opened one. A folded and a literal scalar are joined the same way:
    // the description is popup text on one line, so its breaks carry nothing.
    let mut block: Vec<&str> = Vec::new();
    let mut in_block = false;

    for line in frontmatter.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if in_block && !line.trim().is_empty() {
                block.push(line.trim());
            }
            continue;
        }

        // An unindented line ends whatever block was being read.
        in_block = false;
        if !block.is_empty() {
            description = Some(block.join(" "));
            block.clear();
        }

        let Some((key, value)) = line.split_once(':') else {
            // Not `key: value`; skip the line rather than guess at YAML.
            continue;
        };
        if key.trim() != DESCRIPTION_KEY {
            continue;
        }

        // `>`, `|` and their `-`/`+` chomping variants are YAML block-scalar
        // markers: the value is the lines below, not the marker itself. An
        // empty value is the same situation, since the marker may sit on the
        // next line.
        let value = value.trim();
        if value.is_empty() || matches!(value, ">" | "|" | ">-" | "|-" | ">+" | "|+") {
            in_block = true;
        } else {
            description = Some(unquote(value).to_owned());
        }
    }

    if !block.is_empty() {
        description = Some(block.join(" "));
    }

    description
}

/// The frontmatter block of a document and the body it leaves behind.
///
/// `None` when the document has no frontmatter: either it does not open with
/// `---`, or the block never closes. A half-written header is treated as no
/// header rather than guessed at.
fn frontmatter_and_body(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some((&rest[..end], &rest[end + "\n---\n".len()..]))
}

/// Strip one layer of matching quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|value| value.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Collapse all runs of whitespace to single spaces.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `max` characters, marking that content was dropped.
fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let kept: String = value.chars().take(max).collect();
    format!("{kept}…")
}

/// The body a document's frontmatter leaves behind, or the whole document
/// when there is no frontmatter.
fn body_of(raw: &str) -> &str {
    frontmatter_and_body(raw).map_or(raw, |(_, body)| body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a skill directory under `root` and return its path.
    fn write_skill(root: &Path, name: &str, contents: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), contents).unwrap();
        dir
    }

    fn temp_root(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "alan-skills-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A whole skill file written the way real ones are: frontmatter with a
    /// folded description, then the body.
    fn skill_file(description: &str, body: &str) -> String {
        format!("---\ndescription: >\n  {description}\n---\n{body}\n")
    }

    /// The description of a document whose frontmatter is `block`.
    fn description_of_block(block: &str) -> Option<String> {
        description_of(&format!("---\n{block}\n---\nbody\n"))
    }

    #[test]
    fn a_folded_description_becomes_one_line() {
        assert_eq!(
            description_of_block(concat!(
                "description: >\n",
                "  Ultra-compressed review comments.\n",
                "  Keeps the actionable signal.\n",
                "  Use when reviewing a diff.\n",
            ))
            .unwrap(),
            "Ultra-compressed review comments. Keeps the actionable signal. Use when reviewing a diff."
        );
    }

    /// A folded and a literal scalar are joined the same way: the description
    /// is popup text on one line, so its line breaks carry nothing.
    #[test]
    fn a_literal_block_keeps_its_line_breaks() {
        assert_eq!(
            description_of_block("description: |\n  first line\n  second line\n").unwrap(),
            "first line second line"
        );
    }

    #[test]
    fn quoted_values_lose_their_quotes() {
        assert_eq!(
            description_of_block("description: \"Use when shipping, not before\"\n").unwrap(),
            "Use when shipping, not before"
        );
    }

    /// An unmodelled key arrives with indented children, which must not be
    /// mistaken for the block scalar of the real key.
    #[test]
    fn unknown_nested_keys_are_skipped_whole() {
        assert_eq!(
            description_of_block(concat!(
                "allowed-tools: \"Read, Write\"\n",
                "hooks:\n",
                "  PreToolUse:\n",
                "    - matcher: Write\n",
                "      command: echo hi\n",
                "description: Use when shipping\n",
            ))
            .unwrap(),
            "Use when shipping"
        );
    }

    /// A document that does not open with `---` has no frontmatter at all.
    #[test]
    fn a_document_without_frontmatter_has_no_description() {
        assert_eq!(description_of("just a body\n"), None);
    }

    /// An unclosed header is treated as body rather than guessed at, so it
    /// yields no description.
    #[test]
    fn an_unclosed_frontmatter_yields_no_description() {
        assert_eq!(description_of("---\ndescription: x\n"), None);
    }

    #[test]
    fn load_all_reads_description_and_body() {
        let root = temp_root("load");
        write_skill(
            &root,
            "deploy",
            &skill_file("Use when shipping", "Run the release checklist."),
        );

        let skills = load_all(std::slice::from_ref(&root));

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "Use when shipping");
        assert_eq!(skills[0].instructions, "Run the release checklist.");
        assert!(
            skills[0]
                .file_path
                .as_ref()
                .unwrap()
                .ends_with("deploy/SKILL.md")
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// The directory name is what a token resolves against, so it wins over
    /// whatever the frontmatter claims.
    #[test]
    fn the_directory_name_is_authoritative() {
        let root = temp_root("dir-name");
        write_skill(
            &root,
            "deploy",
            "---\nname: something-else\ndescription: Use when shipping\n---\nbody\n",
        );

        let skills = load_all(std::slice::from_ref(&root));

        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "Use when shipping");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_skill_without_a_description_is_skipped() {
        let root = temp_root("no-desc");
        write_skill(&root, "good", &skill_file("Use when shipping", "body"));
        write_skill(
            &root,
            "bad",
            "---\nname: bad\n---\nbody with no description\n",
        );

        let skills = load_all(std::slice::from_ref(&root));

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_broken_skill_does_not_cost_the_others() {
        let root = temp_root("broken");
        write_skill(&root, "good", &skill_file("Use when shipping", "body"));
        std::fs::create_dir_all(root.join("empty")).unwrap();

        let skills = load_all(std::slice::from_ref(&root));

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_root_yields_no_skills() {
        let missing = std::env::temp_dir().join("alan-skills-does-not-exist-12345");

        assert!(load_all(&[missing]).is_empty());
    }

    #[test]
    fn an_earlier_root_shadows_a_later_one() {
        let project = temp_root("project");
        let personal = temp_root("personal");
        write_skill(&project, "deploy", &skill_file("project", "body"));
        write_skill(&personal, "deploy", &skill_file("personal", "body"));
        write_skill(&personal, "review", &skill_file("personal only", "body"));

        let skills = load_all(&[project.clone(), personal.clone()]);

        assert_eq!(skills.len(), 2);
        let deploy = skills.iter().find(|s| s.name == "deploy").unwrap();
        assert_eq!(deploy.description, "project");
        assert!(skills.iter().any(|s| s.name == "review"));

        std::fs::remove_dir_all(&project).ok();
        std::fs::remove_dir_all(&personal).ok();
    }

    #[test]
    fn the_shared_root_is_under_the_agents_directory() {
        assert_eq!(
            shared_root(Path::new("/home/dev")),
            PathBuf::from("/home/dev/.agents/skills")
        );
    }

    #[test]
    fn the_shared_root_is_scanned_last_and_shadowed() {
        let home = temp_root("home");
        let cwd = home.join("project");
        // The three roots under test, built the way `main` builds them.
        let project_skills = project_root(&cwd);
        let personal_skills = personal_root(&home.join(".alan"));
        let shared_skills = shared_root(&home);

        write_skill(&project_skills, "deploy", &skill_file("project", "body"));
        write_skill(&personal_skills, "deploy", &skill_file("personal", "body"));
        write_skill(&shared_skills, "deploy", &skill_file("shared", "body"));
        write_skill(
            &shared_skills,
            "no-ai-slop",
            &skill_file("shared only", "body"),
        );

        let skills = load_all(&[project_skills, personal_skills, shared_skills]);

        assert_eq!(skills.len(), 2);
        assert_eq!(
            skills
                .iter()
                .find(|s| s.name == "deploy")
                .unwrap()
                .description,
            "project"
        );
        assert!(skills.iter().any(|s| s.name == "no-ai-slop"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn skills_are_loaded_in_a_stable_order() {
        let root = temp_root("order");
        for name in ["zebra", "alpha", "middle"] {
            write_skill(&root, name, &skill_file("d", "body"));
        }

        let names: Vec<String> = load_all(std::slice::from_ref(&root))
            .into_iter()
            .map(|skill| skill.name)
            .collect();

        assert_eq!(names, vec!["alpha", "middle", "zebra"]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_oversized_description_is_truncated() {
        let root = temp_root("long-desc");
        let long = "x".repeat(MAX_DESCRIPTION + 100);
        write_skill(&root, "big", &skill_file(&long, "body"));

        let skills = load_all(std::slice::from_ref(&root));

        assert_eq!(skills[0].description.chars().count(), MAX_DESCRIPTION + 1);
        assert!(skills[0].description.ends_with('…'));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_oversized_body_is_truncated() {
        let root = temp_root("long-body");
        let long = "y".repeat(MAX_INSTRUCTIONS + 100);
        write_skill(&root, "big", &skill_file("desc", &long));

        let skills = load_all(std::slice::from_ref(&root));

        assert_eq!(skills[0].instructions.chars().count(), MAX_INSTRUCTIONS + 1);

        std::fs::remove_dir_all(&root).ok();
    }

    fn catalog() -> Vec<Skill> {
        vec![
            Skill {
                name: "deploy".into(),
                description: "Use when shipping".into(),
                instructions: "release checklist".into(),
                disable_model_invocation: false,
                file_path: None,
            },
            Skill {
                name: "review".into(),
                description: "Use when reviewing".into(),
                instructions: "review rules".into(),
                disable_model_invocation: false,
                file_path: None,
            },
        ]
    }

    #[test]
    fn a_token_resolves_to_its_skill() {
        let catalog = catalog();

        let resolved = resolve_skills(&catalog, "please use #deploy now");

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "deploy");
    }

    #[test]
    fn text_without_a_token_resolves_to_nothing() {
        let catalog = catalog();

        assert!(resolve_skills(&catalog, "no tokens here").is_empty());
        assert!(resolve_skills(&catalog, "issue #42 is broken").is_empty());
    }

    #[test]
    fn an_unknown_token_resolves_to_nothing() {
        let catalog = catalog();

        assert!(resolve_skills(&catalog, "use #depoy").is_empty());
    }

    #[test]
    fn trailing_punctuation_does_not_break_a_token() {
        let catalog = catalog();

        for text in ["use #deploy.", "use #deploy,", "use #deploy!", "(#deploy)"] {
            let resolved = resolve_skills(&catalog, text);
            assert_eq!(resolved.len(), 1, "failed for {text:?}");
            assert_eq!(resolved[0].name, "deploy");
        }
    }

    #[test]
    fn tokens_resolve_once_each_in_first_mention_order() {
        let catalog = catalog();

        let resolved = resolve_skills(&catalog, "#review then #deploy then #review again");

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "review");
        assert_eq!(resolved[1].name, "deploy");
    }

    #[test]
    fn a_bare_hash_is_not_a_token() {
        let catalog = catalog();

        assert!(resolve_skills(&catalog, "just a # here").is_empty());
    }
}
