//! Model and provider commands: `/models`, `/model-providers`,
//! `/provider`, `/local`, and the model picker overlays.

use crate::core::SlashCommand;
use crate::core::server_tools::server_tools;
use crate::core::settings::{self, PatchSettings};
use crate::root::AlanAction;
use alan_tui::TaskError;
use alan_tui::context::Context;
use providers::{
    Model, ModelInfo, ModelOptions, OpenRouterOptions, Provider, ProviderId, ProviderRegistry,
    bind_local_model, bind_model,
};
use std::sync::Arc;

use super::{LocalPick, ProviderPick};
use crate::views::components::{SearchListEvent, SearchListOverlay};

use super::{ChatView, parse_provider_order, persist_provider_order};

use crate::core::custom_providers::{CustomProviderEntry, ProviderName};
use crate::local_model_overlay::LocalModelOverlay;
use crate::provider_overlay::ProviderOverlay;

/// Provider id reserved for locally-hosted models.
const LOCAL_PROVIDER_ID: &str = "local";

/// `/models`: open the model picker overlay.
pub(crate) fn open_models_picker(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>) {
    open_model_picker(view, cx, "Select Model".to_owned(), None);
}

/// The model picker. With `replacing`, that provider's models are hidden and
/// the provider is removed once the switch to the chosen model succeeds.
fn open_model_picker(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    title: String,
    replacing: Option<ProviderName>,
) {
    let providers = Arc::clone(&view.providers);
    let hidden = replacing.as_ref().map(ProviderName::id);
    let picker = cx.open_overlay(SearchListOverlay::new(
        title,
        model_labels(&picker_models(&providers, hidden.as_ref())),
    ));

    let providers_for_fetch = Arc::clone(&providers);
    let providers_for_items = Arc::clone(&providers);
    let hidden_for_items = hidden.clone();
    let _ = cx.spawn(
        async move {
            fetch_all_models(&providers_for_fetch).await;
            Ok(())
        },
        move |result, _view, cx| {
            if result.is_ok() {
                let items = model_labels(&picker_models(
                    &providers_for_items,
                    hidden_for_items.as_ref(),
                ));
                let _ = cx.update(picker, |p| p.set_items(items));
            }
        },
    );

    let custom_providers = Arc::clone(&view.custom_providers);
    view.model_subscription = Some(cx.subscribe::<SearchListEvent, SearchListOverlay, _>(
        picker,
        move |event, view, _picker, cx| {
            let SearchListEvent::Chosen(index) = event else {
                return;
            };
            let Some(model_info) = picker_models(&providers, hidden.as_ref())
                .into_iter()
                .nth(*index)
            else {
                return;
            };
            let agent = view.controller.agent();
            let providers = Arc::clone(&providers);
            let custom_providers = Arc::clone(&custom_providers);
            let replacing = replacing.clone();
            let _ = cx.spawn(
                async move {
                    let mut options = agent.model_options().await;
                    let settings = settings::get_settings()
                        .await
                        .map_err(|e| TaskError(e.into()))?;
                    options.extensions = settings.extensions(&model_info.id);

                    let model = bind_model_info(&providers, &model_info, options)?;
                    let name = model_info.name.clone();
                    let max_context = model_info.context_length;
                    let reasoning_effort = model.reasoning_effort();

                    agent
                        .set_model(model)
                        .await
                        .map_err(|e| TaskError(e.into()))?;
                    persist_model(&model_info.id, &model_info.provider)
                        .await
                        .map_err(|e| TaskError(e.into()))?;

                    // Only after the switch: until then the old provider's
                    // key is still the one requests go out with.
                    let removal = match replacing {
                        Some(replaced) => {
                            let result = custom_providers
                                .remove(&replaced, &model_info.provider)
                                .await;
                            Some(result.map(|()| replaced).map_err(|e| format!("{e:#}")))
                        }
                        None => None,
                    };
                    Ok((name, max_context, reasoning_effort, removal))
                },
                move |result, view, cx| {
                    match result {
                        Ok((name, max_context, reasoning_effort, removal)) => {
                            view.controller.set_max_context(max_context);
                            view.controller.set_reasoning_effort(reasoning_effort);
                            view.controller.apply_model_switch(name);
                            match removal {
                                Some(Ok(replaced)) => view
                                    .controller
                                    .push_info(format!("removed provider: {replaced}")),
                                Some(Err(error)) => view.controller.push_info(format!(
                                    "switched models, but failed to remove the provider: {error}"
                                )),
                                None => {}
                            }
                        }
                        Err(e) => view.controller.apply_model_switch_failed(e.to_string()),
                    }
                    cx.notify();
                },
            );
        },
    ));
}

/// `/providers`: apply a provider order.
pub(crate) fn apply_model_provider(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    text: &str,
) {
    if view.controller.is_busy() {
        return;
    }
    let agent = view.controller.agent();

    let provider_order = match SlashCommand::parse_with_args(text).map(|(_, args)| args) {
        Some(args) if args.trim().eq_ignore_ascii_case("none") => Ok(Vec::new()),
        Some(args) => parse_provider_order(args),
        None => return,
    };
    let provider_order = match provider_order {
        Ok(order) => order,
        Err(error) => {
            view.controller.push_info(format!(
                "usage: /providers <provider1,provider2,...>: {error}"
            ));
            return;
        }
    };

    cx.spawn(
        async move {
            let model_id = agent.info().await.id;
            let mut extensions = agent.model_options().await.extensions;
            extensions.insert(OpenRouterOptions {
                provider_order: provider_order.clone(),
            });
            agent
                .set_extensions(extensions)
                .await
                .map_err(|error| TaskError(Box::new(error)))?;

            persist_provider_order(&model_id, &provider_order)
                .await
                .map_err(|error| TaskError(error.into()))?;

            Ok::<_, TaskError>(if provider_order.is_empty() {
                "provider order cleared (using default)".to_owned()
            } else {
                format!("provider order set to {}", provider_order.join(", "))
            })
        },
        move |result, view, cx| match result {
            Ok(msg) => {
                view.controller.push_info(msg);
                cx.notify();
            }
            Err(error) => {
                view.controller
                    .push_info(format!("failed to set provider order: {error}"));
                cx.notify();
            }
        },
    );
}

/// `/local`: dispatch local model sub-commands.
pub(crate) fn handle_local_command(
    view: &mut ChatView,
    text: &str,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    let args = SlashCommand::parse_with_args(text).map(|(_, a)| a.trim().to_owned());
    match args.as_deref() {
        Some("add") => open_local_model_overlay(view, cx, None),
        Some("remove") => open_local_remove_picker(view, cx),
        Some("edit") => open_local_edit_picker(view, cx),
        _ => view
            .controller
            .push_info("usage: /local <add|remove|edit>".to_owned()),
    }
}

/// `/provider`: add, edit or remove a custom provider.
pub(crate) fn handle_provider_command(
    view: &mut ChatView,
    text: &str,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    let args = SlashCommand::parse_with_args(text).map(|(_, a)| a.trim().to_owned());
    match args.as_deref() {
        Some("add") => {
            cx.open_overlay(ProviderOverlay::add(Arc::clone(&view.custom_providers)));
        }
        Some("edit") => open_provider_picker(view, cx, ProviderPick::Edit),
        Some("remove") => open_provider_picker(view, cx, ProviderPick::Remove),
        _ => view
            .controller
            .push_info("usage: /provider <add|edit|remove>".to_owned()),
    }
}

/// List saved providers in a picker, then edit or remove the chosen one.
fn open_provider_picker(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    pick: ProviderPick,
) {
    let providers = Arc::clone(&view.custom_providers);
    cx.spawn(
        async move { providers.entries().await.map_err(super::to_task) },
        move |result, view, cx| {
            let entries = match result {
                Ok(entries) if entries.is_empty() => {
                    view.controller
                        .push_info("no providers yet; add one with /provider add".to_owned());
                    cx.notify();
                    return;
                }
                Ok(entries) => entries,
                Err(error) => {
                    view.controller
                        .push_info(format!("failed to read providers: {error}"));
                    cx.notify();
                    return;
                }
            };
            let title = match pick {
                ProviderPick::Edit => "Edit Provider",
                ProviderPick::Remove => "Remove Provider",
            };
            let labels = entries
                .iter()
                .map(|entry| format!("{} — {}", entry.name, entry.url))
                .collect();
            let picker = cx.open_overlay(SearchListOverlay::new(title, labels));
            cx.subscribe_once::<SearchListEvent, SearchListOverlay, _>(
                picker,
                move |event, view, _picker, cx| {
                    let SearchListEvent::Chosen(index) = event else {
                        return;
                    };
                    let Some(entry) = entries.get(*index).cloned() else {
                        return;
                    };
                    match pick {
                        ProviderPick::Edit => edit_provider(view, cx, entry),
                        ProviderPick::Remove => remove_provider(view, cx, entry.name),
                    }
                },
            );
        },
    );
}

fn edit_provider(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    entry: CustomProviderEntry,
) {
    let providers = Arc::clone(&view.custom_providers);
    cx.spawn(
        async move {
            let key = providers.key(&entry.name).await.map_err(super::to_task)?;
            Ok((entry, key))
        },
        move |result, view, cx| match result {
            Ok((entry, key)) => {
                cx.open_overlay(ProviderOverlay::edit(
                    Arc::clone(&view.custom_providers),
                    entry,
                    key,
                ));
            }
            Err(error) => {
                view.controller
                    .push_info(format!("failed to read the provider's key: {error}"));
                cx.notify();
            }
        },
    );
}

/// The provider of the running model is swapped out first: the model picker
/// opens without its models, and it goes once the switch succeeds.
fn remove_provider(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    name: ProviderName,
) {
    let agent = view.controller.agent();
    cx.spawn(
        async move { Ok::<_, TaskError>(agent.info().await.provider) },
        move |result, view, cx| {
            let Ok(active) = result else {
                return;
            };
            if active == name.id() {
                open_model_picker(view, cx, format!("Replace {name} with"), Some(name));
            } else {
                remove_idle_provider(view, cx, name, active);
            }
        },
    );
}

fn remove_idle_provider(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    name: ProviderName,
    active: ProviderId,
) {
    let providers = Arc::clone(&view.custom_providers);
    cx.spawn(
        async move {
            providers
                .remove(&name, &active)
                .await
                .map_err(super::to_task)?;
            Ok(name)
        },
        |result, view, cx| {
            view.controller.push_info(match result {
                Ok(name) => format!("removed provider: {name}"),
                Err(error) => format!("failed to remove provider: {error}"),
            });
            cx.notify();
        },
    );
}

/// Open the local model overlay (for adding or editing).
pub(crate) fn open_local_model_overlay(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    edit_entry: Option<providers::LocalModelEntry>,
) {
    let Some(local_provider) = view.providers.local() else {
        view.controller
            .push_info("Local provider not available".to_owned());
        return;
    };
    cx.open_overlay(LocalModelOverlay::new(local_provider, edit_entry));
}

/// Open a picker to remove a local model.
fn open_local_remove_picker(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>) {
    open_local_picker(
        view,
        cx,
        "Remove Local Model",
        "No local models to remove.",
        LocalPick::Remove,
    );
}

/// Open a picker to edit a local model.
fn open_local_edit_picker(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>) {
    open_local_picker(
        view,
        cx,
        "Edit Local Model",
        "No local models to edit.",
        LocalPick::Edit,
    );
}

/// Open a `SearchListOverlay` over the local catalog and run `on_pick` with the
/// chosen entry's index. Shared by the remove and edit flows.
fn open_local_picker(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    title: &str,
    empty_message: &str,
    pick: LocalPick,
) {
    let Some(local) = view.providers.local() else {
        view.controller.push_info(empty_message.to_owned());
        return;
    };
    let models: Vec<String> = local
        .models()
        .iter()
        .map(|m| format!("local — {}", m.name))
        .collect();
    if models.is_empty() {
        view.controller.push_info(empty_message.to_owned());
        return;
    }
    let picker = cx.open_overlay(SearchListOverlay::new(title, models));
    let local = local.clone();
    cx.subscribe_once::<SearchListEvent, SearchListOverlay, _>(
        picker,
        move |event, _view, _picker, cx| {
            let SearchListEvent::Chosen(index) = event else {
                return;
            };
            let Some(model_info) = local.models().into_iter().nth(*index) else {
                return;
            };
            match &pick {
                LocalPick::Remove => {
                    let local = Arc::clone(&local);
                    let model_id = model_info.id.clone();
                    let display_id = model_id.clone();
                    cx.spawn(
                        async move {
                            local
                                .remove_model(&model_id)
                                .await
                                .map_err(|e| TaskError(e.to_string().into()))?;
                            Ok::<(), TaskError>(())
                        },
                        move |result, view, _cx| match result {
                            Ok(()) => {
                                view.controller
                                    .push_info(format!("Removed local model: {display_id}"));
                            }
                            Err(e) => view.controller.push_info(format!("Failed to remove: {e}")),
                        },
                    );
                }
                LocalPick::Edit => {
                    let local = Arc::clone(&local);
                    cx.spawn(
                        async move {
                            local.find_entry(&model_info.id).ok_or_else(|| {
                                TaskError(
                                    format!("Local model not found: {}", model_info.id).into(),
                                )
                            })
                        },
                        move |result, view, cx| match result {
                            Ok(entry) => open_local_model_overlay(view, cx, Some(entry)),
                            Err(e) => view.controller.push_info(e.to_string()),
                        },
                    );
                }
            }
        },
    );
}

pub(crate) async fn fetch_all_models(providers: &ProviderRegistry) {
    for provider in providers.providers() {
        if let Err(e) = provider.fetch_models().await {
            tracing::warn!(
                "Failed to fetch models for provider {:?}: {}",
                provider.id(),
                e
            );
        }
    }
}

pub(crate) fn all_models(providers: &ProviderRegistry) -> Vec<ModelInfo> {
    providers
        .providers()
        .iter()
        .flat_map(|p| p.models())
        .collect()
}

/// The models a picker offers: every provider's, minus `hidden`'s.
fn picker_models(providers: &ProviderRegistry, hidden: Option<&ProviderId>) -> Vec<ModelInfo> {
    all_models(providers)
        .into_iter()
        .filter(|model| Some(&model.provider) != hidden)
        .collect()
}

fn model_labels(models: &[ModelInfo]) -> Vec<String> {
    models
        .iter()
        .map(|m| format!("{} — {}", m.provider, m.name))
        .collect()
}

pub(crate) fn server_tools_for_provider(
    providers: &ProviderRegistry,
    provider_id: &str,
    web_fetch: bool,
    web_search: bool,
) -> Vec<llm::ServerTool> {
    providers
        .get(&ProviderId::new(provider_id))
        .map_or_else(Vec::new, |provider| {
            server_tools(provider.server_tools(), web_fetch, web_search)
        })
}

/// Bind a catalog entry to a live model. Local entries bind through the
/// registry's local provider; everything else through the named provider.
pub(crate) fn bind_model_info(
    providers: &ProviderRegistry,
    model_info: &ModelInfo,
    options: ModelOptions,
) -> Result<Model, TaskError> {
    if model_info.provider.0 == LOCAL_PROVIDER_ID {
        let local = providers
            .local()
            .ok_or_else(|| TaskError("local provider not available".into()))?;
        let entry = local
            .find_entry(&model_info.id)
            .ok_or_else(|| TaskError(format!("local model not found: {}", model_info.id).into()))?;
        return bind_local_model(&entry, options).map_err(|e| TaskError(e.into()));
    }

    let provider = providers
        .get(&model_info.provider)
        .ok_or_else(|| TaskError(format!("provider not found: {}", model_info.provider).into()))?;
    bind_model(provider.as_ref(), &model_info.id, options).map_err(|e| TaskError(e.into()))
}

pub(crate) async fn persist_model(model_id: &str, provider_id: &ProviderId) -> anyhow::Result<()> {
    super::apply_settings_patch(PatchSettings {
        model: Some(model_id.to_owned()),
        provider: Some(provider_id.to_string()),
        ..PatchSettings::default()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{CustomProvider, InMemoryCredentialStore};

    fn provider(name: &str, model: &str) -> Arc<dyn Provider> {
        Arc::new(CustomProvider::new(
            ProviderId::new(name),
            "http://localhost:11434/v1",
            Arc::new(InMemoryCredentialStore::new()),
            vec![ModelInfo::new(model, ProviderId::new(name))],
        ))
    }

    #[test]
    fn picker_models_hides_only_the_replaced_provider() {
        let registry =
            ProviderRegistry::new([provider("grok", "grok-4"), provider("groq", "llama")]);

        let all: Vec<_> = picker_models(&registry, None)
            .into_iter()
            .map(|m| m.id)
            .collect();
        let without_grok: Vec<_> = picker_models(&registry, Some(&ProviderId::new("grok")))
            .into_iter()
            .map(|m| m.id)
            .collect();

        assert_eq!(all, ["grok-4", "llama"]);
        assert_eq!(without_grok, ["llama"]);
    }
}
