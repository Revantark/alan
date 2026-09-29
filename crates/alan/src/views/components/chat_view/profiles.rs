//! `/profile`: save, apply, and delete saved model profiles, and the
//! saved-profile picker.

use crate::core::SlashCommand;
use crate::core::profile::{self, Profile, ProfileStore};
use crate::core::server_tools::web_flags;
use crate::core::settings::{self, PatchSettings};
use crate::root::AlanAction;
use crate::views::components::{SearchListEvent, SearchListOverlay};
use alan_tui::TaskError;
use alan_tui::context::Context;
use providers::ProviderId;
use std::sync::Arc;

use super::ChatView;
use super::models::{all_models, bind_model_info, fetch_all_models, server_tools_for_provider};

#[derive(Clone, Copy)]
pub(crate) enum ProfileOperation {
    Apply,
    Delete,
}

/// The profile store at the default location.
fn profile_store() -> Result<ProfileStore, TaskError> {
    Ok(ProfileStore::new(
        profile::default_profiles_path().map_err(super::to_task)?,
    ))
}

/// `/profile` as an already-dispatched action: the operation the picker runs
/// on the chosen profile, or a name to save under.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProfileRequest {
    Apply,
    Delete,
    Save(String),
    Usage,
}

/// Parse the arguments after `/profile` into a request. Kept separate from
/// the dispatch so the parsing is testable on its own.
fn parse_profile_request(args: &str) -> ProfileRequest {
    let args = args.trim();
    // The command parser strips leading whitespace, so this cleanly splits
    // the subcommand from its argument; a subcommand with no argument
    // yields an empty `rest`.
    let (subcommand, rest) = match args.split_once(char::is_whitespace) {
        Some((subcommand, rest)) => (subcommand, rest.trim()),
        None => (args, ""),
    };

    if subcommand.eq_ignore_ascii_case("save") {
        return ProfileRequest::Save(rest.to_owned());
    }
    if rest.is_empty() {
        if subcommand.eq_ignore_ascii_case("delete") {
            return ProfileRequest::Delete;
        }
        if subcommand.is_empty() {
            return ProfileRequest::Apply;
        }
    }
    ProfileRequest::Usage
}

/// `/profile`: save a profile, or open the saved-profile picker.
pub(crate) fn handle_profile_command(
    view: &mut ChatView,
    text: &str,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    if reject_while_busy(view) {
        return;
    }

    let request = SlashCommand::parse_with_args(text)
        .map(|(_, args)| parse_profile_request(args))
        .unwrap_or(ProfileRequest::Apply);

    match request {
        ProfileRequest::Apply => open_profile_picker(cx, ProfileOperation::Apply),
        ProfileRequest::Delete => open_profile_picker(cx, ProfileOperation::Delete),
        ProfileRequest::Save(name) => save_profile(view, cx, &name),
        ProfileRequest::Usage => view
            .controller
            .push_info("usage: /profile [save <name>|delete]".to_owned()),
    }
}

pub(crate) fn reject_while_busy(view: &mut ChatView) -> bool {
    if !view.controller.is_busy() {
        return false;
    }
    view.controller
        .push_info("profiles are unavailable while streaming".to_owned());
    true
}

pub(crate) fn open_profile_picker(
    cx: &mut Context<'_, ChatView, AlanAction>,
    operation: ProfileOperation,
) {
    let title = match operation {
        ProfileOperation::Apply => "Select Profile",
        ProfileOperation::Delete => "Delete Profile",
    };

    let _ = cx.spawn(
        async move {
            profile_store()?
                .load()
                .await
                .map(|profiles| profiles.into_iter().collect::<Vec<_>>())
                .map_err(super::to_task)
        },
        move |result, _view, cx| match result {
            Ok(profiles) => {
                let items = profiles.iter().map(|(name, _)| name.clone()).collect();

                let mut picker = SearchListOverlay::new(title, items);
                if profiles.is_empty() {
                    picker.set_empty_message("No saved profiles");
                }

                let popup = cx.open_overlay(picker);

                cx.subscribe_once::<SearchListEvent, SearchListOverlay, _>(
                    popup,
                    move |event, view, _picker, cx| {
                        let SearchListEvent::Chosen(index) = event else {
                            return;
                        };
                        let Some((name, profile)) = profiles.get(*index).cloned() else {
                            return;
                        };
                        match operation {
                            ProfileOperation::Apply => apply_profile(view, cx, name, profile),
                            ProfileOperation::Delete => delete_profile(cx, name),
                        }
                    },
                );
            }
            Err(error) => {
                let mut picker = SearchListOverlay::new(title, Vec::new());
                picker.set_empty_message(format!("unable to load profiles: {error}"));

                cx.open_overlay(picker);
            }
        },
    );
}

fn save_profile(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>, name: &str) {
    if name.trim().is_empty() {
        view.controller
            .push_info("usage: /profile save <name>".to_owned());
        return;
    }

    store_profile(view, cx, name.to_owned());
}

async fn profile_from_runtime(agent: &Arc<agent::Agent>) -> Profile {
    let info = agent.info().await;
    let options = agent.model_options().await;
    let (web_fetch, web_search) = web_flags(&options.server_tools);

    Profile {
        provider: info.provider.0,
        model: info.id,
        reasoning: options.reasoning_effort,
        web_fetch,
        web_search,
    }
}

fn apply_profile(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    name: String,
    profile: Profile,
) {
    let providers = Arc::clone(&view.providers);
    let agent = view.controller.agent();
    cx.spawn(
        async move {
            let mut options = agent.model_options().await;
            let settings = settings::get_settings().await.map_err(super::to_task)?;
            options.provider_order = settings.provider_order(&profile.model);
            options.reasoning_effort = profile.reasoning;
            options.server_tools = server_tools_for_provider(
                &providers,
                &profile.provider,
                profile.web_fetch,
                profile.web_search,
            );

            let profile_model = resolve_profile_model(&providers, &profile).await?;

            let model = bind_model_info(&providers, &profile_model, options)?;

            let reasoning = model.reasoning_effort();
            let model_name = profile_model.name.clone();
            let max_context = profile_model.context_length;
            agent
                .set_model(model)
                .await
                .map_err(|error| TaskError(error.into()))?;

            persist_profile_settings(&profile, &name)
                .await
                .map_err(super::to_task)?;

            Ok((name, model_name, max_context, reasoning))
        },
        move |result, view, cx| {
            match result {
                Ok((profile_name, model_name, max_context, reasoning)) => {
                    view.controller.set_max_context(max_context);
                    view.controller.set_reasoning_effort(reasoning);
                    view.controller.apply_model_switch(model_name);
                    view.controller
                        .push_info(format!("profile applied: {profile_name}"));
                }
                Err(error) => view.controller.apply_model_switch_failed(error.to_string()),
            }
            cx.notify();
        },
    );
}

fn delete_profile(cx: &mut Context<'_, ChatView, AlanAction>, name: String) {
    cx.spawn(
        async move {
            let deleted = profile_store()?
                .delete(&name)
                .await
                .map_err(super::to_task)?;

            if deleted {
                super::settings_store()?
                    .update(|mut settings| {
                        if settings.active_profile.as_deref() == Some(name.as_str()) {
                            settings.active_profile = None;
                        }

                        settings
                    })
                    .await
                    .map_err(super::to_task)?;
            }

            Ok::<_, TaskError>((name, deleted))
        },
        |result, view, cx| {
            view.controller.push_info(match result {
                Ok((name, true)) => format!("deleted profile: {name}"),
                Ok((name, false)) => format!("profile not found: {name}"),
                Err(error) => format!("failed to delete profile: {error}"),
            });
            cx.notify();
        },
    );
}

/// Capture the running model's settings and save them as a new profile.
fn store_profile(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>, name: String) {
    let agent = view.controller.agent();

    cx.spawn(
        async move {
            let profile = profile_from_runtime(&agent).await;

            profile_store()?
                .save_new(&name, profile)
                .await
                .map_err(super::to_task)?;

            Ok::<_, TaskError>(name)
        },
        |result, view, cx| {
            view.controller.push_info(match result {
                Ok(name) => format!("saved profile: {name}"),
                Err(error) => format!("failed to save profile: {error}"),
            });
            cx.notify();
        },
    );
}

/// Find a profile's model in the already-cached catalog.
fn find_profile_model(
    providers: &providers::ProviderRegistry,
    profile: &Profile,
) -> Option<providers::ModelInfo> {
    let profile_provider = ProviderId::new(&profile.provider);

    all_models(providers)
        .into_iter()
        .find(|info| info.provider == profile_provider && info.id == profile.model)
}

/// Resolve a profile's model, refreshing the catalog once if it isn't cached
/// yet, so a profile saved before a provider added the model still applies.
async fn resolve_profile_model(
    providers: &providers::ProviderRegistry,
    profile: &Profile,
) -> Result<providers::ModelInfo, TaskError> {
    if let Some(found) = find_profile_model(providers, profile) {
        return Ok(found);
    }

    fetch_all_models(providers).await;

    find_profile_model(providers, profile).ok_or_else(|| {
        TaskError(
            format!(
                "model {} is unavailable for provider {}",
                profile.model, profile.provider
            )
            .into(),
        )
    })
}

async fn persist_profile_settings(profile: &Profile, name: &str) -> anyhow::Result<()> {
    let patch = PatchSettings {
        provider: Some(profile.provider.clone()),
        model: Some(profile.model.clone()),
        reasoning: Some(profile.reasoning),
        web_fetch: Some(profile.web_fetch),
        web_search: Some(profile.web_search),
        ..PatchSettings::default()
    };
    let name = name.to_owned();

    super::settings_store()?
        .update(move |mut settings| {
            settings.apply_patch(patch);
            settings.active_profile = Some(name);
            settings
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::{ProfileRequest, parse_profile_request};

    #[test]
    fn bare_profile_applies() {
        assert_eq!(parse_profile_request(""), ProfileRequest::Apply);
        assert_eq!(parse_profile_request("   "), ProfileRequest::Apply);
    }

    #[test]
    fn subcommands_are_case_insensitive() {
        assert_eq!(parse_profile_request("DELETE"), ProfileRequest::Delete);
        assert_eq!(
            parse_profile_request("SAVE myprof"),
            ProfileRequest::Save("myprof".to_owned())
        );
    }

    /// `/profile save` with no name still routes to `save_profile`, which
    /// prints the `/profile save <name>` usage line.
    #[test]
    fn bare_save_carries_an_empty_name() {
        assert_eq!(
            parse_profile_request("save"),
            ProfileRequest::Save(String::new())
        );
    }

    /// Names are taken verbatim as the rest of the line.
    #[test]
    fn save_keeps_the_whole_argument() {
        assert_eq!(
            parse_profile_request("save  my prof  "),
            ProfileRequest::Save("my prof".to_owned())
        );
    }

    /// `delete` takes no argument, so a stray word is a usage error rather
    /// than a silently ignored tail.
    #[test]
    fn argumentless_subcommands_reject_extra_words() {
        assert_eq!(parse_profile_request("delete oops"), ProfileRequest::Usage);
    }

    #[test]
    fn unknown_subcommands_report_usage() {
        assert_eq!(parse_profile_request("nope"), ProfileRequest::Usage);
        assert_eq!(parse_profile_request("nope arg"), ProfileRequest::Usage);
    }
}
