//! Model, provider, and local-model commands: `/models`, `/providers`,
//! `/local`, and the model picker overlays.

use crate::core::SlashCommand;
use crate::core::settings::{self, Settings, SettingsStore};
use crate::root::AlanAction;
use alan_tui::context::Context;
use providers::{ModelInfo, Provider, ProviderId, ProviderRegistry, bind_local_model, bind_model};
use std::sync::Arc;

use super::LocalPick;
use crate::views::components::{ModelPick, ModelsPicker};

use super::{ChatView, parse_provider_order, persist_provider_order};

use crate::local_model_overlay::LocalModelOverlay;

/// `/models`: open the model picker overlay.
pub(crate) fn open_models_picker(view: &mut ChatView, cx: &mut Context<'_, ChatView, AlanAction>) {
    let providers = Arc::clone(&view.providers);
    let picker = cx.open_overlay(ModelsPicker::new("Select Model", model_labels(&providers)));

    let providers_for_fetch = Arc::clone(&providers);
    let providers_for_items = Arc::clone(&providers);
    let _ = cx.spawn(
        async move {
            fetch_all_models(&providers_for_fetch).await;
            Ok(())
        },
        move |result, _view, cx| {
            if result.is_ok() {
                let items = model_labels(&providers_for_items);
                let _ = cx.update(picker, |p| p.set_items(items));
            }
        },
    );

    let providers = Arc::clone(&view.providers);
    let local_provider = view.providers.local();
    view.model_subscription = Some(
        cx.subscribe::<crate::views::components::ModelPick, ModelsPicker, _>(
            picker,
            move |event, view, _picker, cx| {
                let ModelPick::Chosen(index) = event else {
                    return;
                };
                let Some(model_info) = all_models(&providers).into_iter().nth(*index) else {
                    return;
                };
                let Some(agent) = Some(view.controller.agent()) else {
                    return;
                };
                let providers = Arc::clone(&providers);
                let local_provider = local_provider.clone();
                let _ = cx.spawn(
                    async move {
                        let mut options = agent.model_options().await;
                        let settings = settings::get_settings()
                            .await
                            .map_err(|e| alan_tui::TaskError(e.into()))?;
                        options.provider_order = settings.provider_order(&model_info.id);

                        let model = if model_info.provider == ProviderId::new("local") {
                            // Local models bind per-entry from the local store.
                            let Some(local_provider) = local_provider else {
                                return Err(alan_tui::TaskError(
                                    "local provider not available".into(),
                                ));
                            };
                            let entry =
                                local_provider.find_entry(&model_info.id).ok_or_else(|| {
                                    alan_tui::TaskError(
                                        format!("local model not found: {}", model_info.id).into(),
                                    )
                                })?;
                            bind_local_model(&entry, options)
                                .map_err(|e| alan_tui::TaskError(e.into()))?
                        } else {
                            let provider = providers
                                .providers()
                                .iter()
                                .find(|p| p.id() == model_info.provider)
                                .ok_or_else(|| {
                                    alan_tui::TaskError("selected provider is unavailable".into())
                                })?;
                            bind_model(provider.as_ref(), &model_info.id, options)
                                .map_err(|e| alan_tui::TaskError(e.into()))?
                        };
                        let name = model_info.name.clone();
                        let max_context = model_info.context_length;
                        let reasoning_effort = model.reasoning_effort();

                        agent
                            .set_model(model)
                            .await
                            .map_err(|e| alan_tui::TaskError(e.into()))?;
                        persist_model(&model_info.id, &model_info.provider)
                            .await
                            .map_err(|e| alan_tui::TaskError(e.into()))?;
                        Ok((name, max_context, reasoning_effort))
                    },
                    move |result, view, cx| {
                        match result {
                            Ok((name, max_context, reasoning_effort)) => {
                                view.controller.set_max_context(max_context);
                                view.controller.set_reasoning_effort(reasoning_effort);
                                view.controller.apply_model_switch(name);
                            }
                            Err(e) => view.controller.apply_model_switch_failed(e.to_string()),
                        }
                        cx.notify();
                    },
                );
            },
        ),
    );
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
            agent
                .set_provider_order(provider_order.clone())
                .await
                .map_err(|error| alan_tui::TaskError(Box::new(error)))?;

            persist_provider_order(&model_id, &provider_order)
                .await
                .map_err(|error| alan_tui::TaskError(error.into()))?;

            Ok::<_, alan_tui::TaskError>(if provider_order.is_empty() {
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

/// Open a `ModelsPicker` over the local catalog and run `on_pick` with the
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
    let picker = cx.open_overlay(ModelsPicker::new(title, models));
    let local = local.clone();
    cx.subscribe_once::<crate::views::components::ModelPick, ModelsPicker, _>(
        picker,
        move |event, _view, _picker, cx| {
            let ModelPick::Chosen(index) = event else {
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
                                .map_err(|e| alan_tui::TaskError(e.to_string().into()))?;
                            Ok::<(), alan_tui::TaskError>(())
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
                                alan_tui::TaskError(
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

pub(crate) fn model_labels(providers: &ProviderRegistry) -> Vec<String> {
    all_models(providers)
        .into_iter()
        .map(|m| format!("{} — {}", m.provider, m.name))
        .collect()
}

pub(crate) async fn persist_model(model_id: &str, provider_id: &ProviderId) -> anyhow::Result<()> {
    let store = SettingsStore::<Settings>::new(settings::default_settings_path()?);
    let mut settings = store.load().await?.unwrap_or_default();
    settings.model = Some(model_id.to_string());
    settings.provider = Some(provider_id.to_string());

    store.save(&settings).await
}
