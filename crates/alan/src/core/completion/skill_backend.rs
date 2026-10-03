//! Skill completion backend for the v2 [`Completer`].
//!
//! Typing `#` offers the skills the user has attached to this project,
//! described by name. Accepting an item inserts `#<name>` into the editor as
//! ordinary text; the skill itself is resolved when the prompt is submitted,
//! not here, so deleting the token removes the skill.

use super::{
    Accept, CompletionBackendV2, CompletionContext, CompletionItem, CompletionRequest,
    CompletionResult, SkillsContext, ranked_items,
};

/// The skill backend. Stateless: it ranks the context's skills against the
/// request pattern and never reads from disk itself.
pub struct SkillCompleterBackend;

impl CompletionBackendV2 for SkillCompleterBackend {
    fn trigger(&self) -> char {
        '#'
    }

    fn complete(
        &self,
        request: &CompletionRequest,
        context: &dyn CompletionContext,
    ) -> Option<CompletionResult> {
        // Only ever see our own context; a mismatch is a programming error
        // that degrades to "no completion" rather than panicking.
        let context = context.as_any().downcast_ref::<SkillsContext>()?;
        let by_name = |name: &str| {
            context
                .skills
                .iter()
                .find(|skill| skill.name == name)
                .map(|skill| skill.description.as_str())
                .unwrap_or_default()
        };
        // Ranking matches on the name, not on the description: the user types
        // `#name`, and `ranked_items` needs something `AsRef<str>`.
        let names: Vec<&str> = context.skills.iter().map(|s| s.name.as_str()).collect();

        Some(CompletionResult {
            range: request.range.clone(),
            status: context.status.clone(),
            items: ranked_items(&request.pattern, &names, |name: &&str| CompletionItem {
                // The trigger survives the replacement, so the item carries
                // the bare name.
                display: format!("#{name} — {}", by_name(name)),
                replacement: (*name).to_owned(),
                accept: Accept::Complete,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::completion::{CompletionStatus, PathsContext};
    use agent::Skill;

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_owned(),
            description: description.to_owned(),
            instructions: String::new(),
        }
    }

    fn catalog() -> SkillsContext {
        SkillsContext {
            skills: vec![
                skill("deploy", "Use when shipping a release"),
                skill("review", "Use when reviewing a diff"),
            ],
            status: CompletionStatus::Ready,
        }
    }

    fn request(pattern: &str) -> CompletionRequest {
        CompletionRequest {
            trigger: '#',
            pattern: pattern.to_owned(),
            range: 1..1 + pattern.len(),
            row: 0,
        }
    }

    #[test]
    fn a_lone_hash_lists_every_skill() {
        let result = SkillCompleterBackend
            .complete(&request(""), &catalog())
            .unwrap();

        assert_eq!(result.items.len(), 2);
    }

    #[test]
    fn a_pattern_narrows_to_the_matching_skills() {
        let result = SkillCompleterBackend
            .complete(&request("dep"), &catalog())
            .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].replacement, "deploy");
    }

    #[test]
    fn an_item_is_displayed_with_its_description() {
        let result = SkillCompleterBackend
            .complete(&request("deploy"), &catalog())
            .unwrap();

        assert_eq!(
            result.items[0].display,
            "#deploy — Use when shipping a release"
        );
    }

    #[test]
    fn accepting_keeps_the_trigger_and_completes_the_name() {
        let result = SkillCompleterBackend
            .complete(&request("dep"), &catalog())
            .unwrap();

        // The range covers only `dep`, so the `#` in front of it survives.
        assert_eq!(result.range, 1..4);
        assert_eq!(result.items[0].replacement, "deploy");
        assert_eq!(result.items[0].accept, Accept::Complete);
    }

    #[test]
    fn the_context_status_is_reported_while_loading() {
        let context = SkillsContext {
            skills: Vec::new(),
            status: CompletionStatus::Loading,
        };

        let result = SkillCompleterBackend
            .complete(&request(""), &context)
            .unwrap();

        assert_eq!(result.status, CompletionStatus::Loading);
    }

    #[test]
    fn a_mismatched_context_yields_no_completion() {
        let context = PathsContext {
            paths: Vec::new(),
            status: CompletionStatus::Ready,
        };

        assert!(
            SkillCompleterBackend
                .complete(&request(""), &context)
                .is_none()
        );
    }
}
