//! Model and provider commands: `/models`, `/model-providers`,
//! `/provider`, and the model picker overlays.

use crate::core::SlashCommand;
use crate::core::settings::{self, PatchSettings, Settings};
use crate::core::tool_selection::select_tools;
use crate::root::AlanAction;
use alan_tui::TaskError;
use alan_tui::context::Context;
use providers::{
    Model, ModelInfo, ModelOptions, OpenRouterOptions, ProviderId, ProviderRegistry, bind_model,
};
use std::sync::Arc;

use crate::views::components::{SearchListEvent, SearchListOverlay};

use super::{ChatView, parse_provider_order, persist_provider_order};

use crate::core::custom_providers::{CustomProviderEntry, ProviderName};
use crate::provider_overlay::{ProviderOverlay, ProviderSaved};

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
            let replacing = replacing.clone();
            let _ = cx.spawn(
                async move {
                    let settings = settings::get_settings()
                        .await
                        .map_err(|e| TaskError(e.into()))?;
                    let options =
                        switch_options(&settings, &model_info, agent.model_options().await);

                    let model = bind_model_info(&providers, &model_info, options)?;
                    let name = model_info.name.clone();
                    let max_context = model_info.context_length;
                    let reasoning_effort = model.reasoning_effort();

                    let tools = select_tools(
                        &providers,
                        &model_info.provider,
                        settings.web_fetch == Some(true),
                        settings.web_search == Some(true),
                    );
                    agent
                        .set_model(model, Some(tools))
                        .await
                        .map_err(|e| TaskError(e.into()))?;
                    persist_model(&model_info.id, &model_info.provider)
                        .await
                        .map_err(|e| TaskError(e.into()))?;
                    Ok((name, max_context, reasoning_effort))
                },
                move |result, view, cx| {
                    match result {
                        Ok((name, max_context, reasoning_effort)) => {
                            view.controller.set_max_context(max_context);
                            view.controller.set_reasoning_effort(reasoning_effort);
                            view.controller.apply_model_switch(name);
                            if let Some(replaced) = replacing {
                                remove_idle_provider(view, cx, replaced);
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
                only: provider_order.clone(),
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

/// `/provider`: add, edit or remove a custom provider.
pub(crate) fn handle_provider_command(
    view: &mut ChatView,
    text: &str,
    cx: &mut Context<'_, ChatView, AlanAction>,
) {
    let args = SlashCommand::parse_with_args(text).map(|(_, a)| a.trim().to_owned());
    match args.as_deref() {
        Some("add") => {
            let form =
                ProviderOverlay::add(view.custom_provider_store(), Arc::clone(&view.providers));
            open_provider_form(cx, form);
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
    let title = match pick {
        ProviderPick::Edit => "Edit Provider",
        ProviderPick::Remove => "Remove Provider",
    };
    let store = view.custom_provider_store();
    cx.spawn(
        async move { store.entries().await.map_err(super::to_task) },
        move |result, _view, cx| match result {
            Ok(entries) => {
                let labels = entries
                    .iter()
                    .map(|entry| format!("{} — {}", entry.name, entry.url))
                    .collect();
                let mut picker = SearchListOverlay::new(title, labels);
                if entries.is_empty() {
                    picker.set_empty_message("No saved providers");
                }
                let picker = cx.open_overlay(picker);
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
            }
            Err(error) => {
                let mut picker = SearchListOverlay::new(title, Vec::new());
                picker.set_empty_message(format!("unable to load providers: {error}"));
                cx.open_overlay(picker);
            }
        },
    );
}

/// Action chosen from the custom-provider picker.
#[derive(Clone, Copy)]
enum ProviderPick {
    Edit,
    Remove,
}

fn edit_provider(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    entry: CustomProviderEntry,
) {
    let store = view.custom_provider_store();
    cx.spawn(
        async move {
            let key = store.key(&entry.name).await.map_err(super::to_task)?;
            Ok((store, entry, key))
        },
        move |result, view, cx| match result {
            Ok((store, entry, key)) => {
                let form = ProviderOverlay::edit(store, Arc::clone(&view.providers), entry, key);
                open_provider_form(cx, form);
            }
            Err(error) => {
                view.controller
                    .push_info(format!("failed to read the provider's key: {error}"));
                cx.notify();
            }
        },
    );
}

fn open_provider_form(cx: &mut Context<'_, ChatView, AlanAction>, form: ProviderOverlay) {
    let form = cx.open_overlay(form);
    cx.subscribe_once::<ProviderSaved, ProviderOverlay, _>(form, |saved, view, _form, cx| {
        cx.close_overlay();
        view.controller.push_info(format!(
            "saved provider: {} ({} model{}); pick one with /models",
            saved.id,
            saved.models,
            if saved.models == 1 { "" } else { "s" }
        ));
        cx.notify();
        rebind_model(view, cx, saved.id.clone());
    });
}

/// A bound model keeps the provider it was bound from, so after an edit the
/// running model is bound again from the registered replacement.
fn rebind_model(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    provider: ProviderId,
) {
    let agent = view.controller.agent();
    let providers = Arc::clone(&view.providers);
    cx.spawn(
        async move {
            let info = agent.info().await;
            if info.provider != provider {
                return Ok(());
            }
            let settings = settings::get_settings()
                .await
                .map_err(|e| TaskError(e.into()))?;
            let model = bind_model_info(&providers, &info, agent.model_options().await)?;
            let tools = select_tools(
                &providers,
                &info.provider,
                settings.web_fetch == Some(true),
                settings.web_search == Some(true),
            );
            agent
                .set_model(model, Some(tools))
                .await
                .map_err(|e| TaskError(e.into()))
        },
        |result, view, cx| {
            if let Err(error) = result {
                view.controller.push_info(format!(
                    "saved, but the running model still uses the old settings: {error}"
                ));
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
                remove_idle_provider(view, cx, name);
            }
        },
    );
}

fn remove_idle_provider(
    view: &mut ChatView,
    cx: &mut Context<'_, ChatView, AlanAction>,
    name: ProviderName,
) {
    let store = view.custom_provider_store();
    let providers = Arc::clone(&view.providers);
    cx.spawn(
        async move {
            store.remove(&name).await.map_err(super::to_task)?;
            providers.remove(&name.id());
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

/// Options for binding `info` after a model switch: the session's reasoning
/// effort carries over, while provider-specific extensions are recomputed from
/// settings. Enabled tools are recomputed separately via `select_tools`.
pub(crate) fn switch_options(
    settings: &Settings,
    info: &ModelInfo,
    base: ModelOptions,
) -> ModelOptions {
    ModelOptions {
        reasoning_effort: base.reasoning_effort,
        extensions: settings.extensions(&info.id),
    }
}

pub(crate) fn bind_model_info(
    providers: &ProviderRegistry,
    model_info: &ModelInfo,
    options: ModelOptions,
) -> Result<Model, TaskError> {
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
    use crate::core::custom_providers::tests::provider;

    #[test]
    fn picker_models_hides_only_the_replaced_provider() {
        let registry = ProviderRegistry::new([]);
        registry.insert(provider("grok", &["grok-4"]));
        registry.insert(provider("groq", &["llama"]));

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

    /// A switch carries the session's reasoning effort over and recomputes
    /// provider-specific extensions from settings. Enabled tools are computed
    /// separately via `select_tools` (see `core::tool_selection`).
    #[test]
    fn switch_options_carries_reasoning_and_uses_settings_extensions() {
        let settings = Settings {
            reasoning: Some(llm::ReasoningEffort::Low),
            ..Settings::default()
        };
        let base = ModelOptions {
            reasoning_effort: llm::ReasoningEffort::High,
            ..ModelOptions::default()
        };

        let options = switch_options(
            &settings,
            &ModelInfo::new("openai/gpt-4o-mini", ProviderId::new("openrouter")),
            base,
        );

        // The session's reasoning effort wins over the setting.
        assert_eq!(options.reasoning_effort, llm::ReasoningEffort::High);
    }
}
