mod core;
mod local_model_overlay;
mod logging;
mod login_overlay;
mod root;
mod views;

use crate::core::settings::{DEFAULT_MODEL, PatchSettings, Settings, SettingsStore};
use crate::core::{ChatController, SlashCommand};
use llm::ServerTool;
use std::time::Duration;

use agent::{Agent, SessionManager, default_tools};
use alan_tui::Runtime;
use llm::ReasoningEffort;
use providers::{
    FileCredentialStore, GoogleProvider, LocalProvider, ModelOptions, OpenRouterProvider, Provider,
    ProviderRegistry, ZaiProvider, bind_model,
};
use std::path::PathBuf;
use std::sync::Arc;

use crate::logging::init;
use crate::root::{AlanKeyMapper, AlanRoot};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let is_blank = std::env::args().any(|arg| arg == "--blank");
    // Saves the passed envs into settings
    let is_save = std::env::args().any(|arg| arg == "--save");

    let _guard = init().unwrap();

    let store = SettingsStore::<Settings>::new(settings_path()?);
    let persisted = match store.load().await? {
        Some(settings) => settings,
        None => {
            let settings = Settings::with_defaults();
            store.save(&settings).await?;
            settings
        }
    };

    let persisted_model = persisted.model.clone();
    let mut settings = persisted;
    settings.apply_patch(build_env_patch_settings(persisted_model)?);

    if is_save {
        store
            .save(&settings)
            .await
            .expect("failed to save settings");
    }

    let credential_store = Arc::new(FileCredentialStore::new(auth_path()?));
    let local_provider = Arc::new(LocalProvider::new(
        alan_data_dir()?.join("local_models.json"),
    ));
    local_provider.load().await?;
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(ZaiProvider::from_store(credential_store.clone()).build()?),
        Arc::new(GoogleProvider::from_store(credential_store.clone()).build()?),
        Arc::new(OpenRouterProvider::from_store(credential_store.clone()).build()?),
        Arc::clone(&local_provider) as Arc<dyn Provider>,
    ];

    let session_manager = Arc::new(SessionManager::new(sessions_path()?));
    let resumed_session = if let Some(session_id) = configured_session_id()? {
        let cwd = std::env::current_dir()?;
        Some(session_manager.get_session(&session_id, &cwd).await?)
    } else {
        None
    };

    let (_, _, _, model) = if let Some(ref session) = resumed_session {
        let provider_id = &session.provider;
        let provider = providers
            .iter()
            .find(|p| p.id().0 == provider_id.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Session was created for provider {} but that provider is not available",
                    provider_id
                )
            })?;
        let model_id = session.model.clone();
        let server_tools = enabled_server_tools(provider.as_ref(), &settings)?;
        let reasoning_effort = settings.reasoning;
        let model = bind_model(
            provider.as_ref(),
            &model_id,
            ModelOptions {
                server_tools,
                reasoning_effort: reasoning_effort.unwrap_or_default(),
                provider_order: settings.provider_order(&model_id),
            },
        )?;
        (provider_id.as_str(), provider.as_ref(), model_id, model)
    } else {
        let selected_provider_id = settings.provider.as_deref().unwrap_or("openrouter");
        let provider = providers
            .iter()
            .find(|p| p.id().0 == selected_provider_id)
            .ok_or_else(|| anyhow::anyhow!("Provider not found: {selected_provider_id}"))?
            .as_ref();
        let model_id = settings
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.into());
        let server_tools = enabled_server_tools(provider, &settings)?;
        let reasoning_effort = settings.reasoning;
        let model = bind_model(
            provider,
            &model_id,
            ModelOptions {
                server_tools,
                reasoning_effort: reasoning_effort.unwrap_or_default(),
                provider_order: settings.provider_order(&model_id),
            },
        )?;
        (selected_provider_id, provider, model_id, model)
    };

    let registry = Arc::new(ProviderRegistry::with_local_provider(
        providers,
        Arc::clone(&local_provider),
    ));

    let was_resumed = resumed_session.is_some();
    let current_dir = std::env::current_dir()?;
    let mut agent_builder = Agent::builder(model)
        .with_directory(current_dir)
        .with_tools(default_tools())
        .session_manager(session_manager);
    if !is_blank {
        agent_builder = agent_builder.with_default_system_prompt();
    }

    if let Some(session) = resumed_session {
        agent_builder = agent_builder.resume_session(session);
    }
    let agent = agent_builder.build()?;
    let model_name = agent.info().await.name;
    let mut controller = ChatController::new(agent, model_name);
    if was_resumed {
        controller.restore_session_history().await;
    }
    // `Runtime::run` consumes the root, so keep the agent for the saved-session
    // message printed after the TUI exits.
    let agent = controller.agent();
    let result = Runtime::builder(AlanRoot::new(
        controller,
        registry,
        credential_store,
    ))
    .key_mapper(AlanKeyMapper)
    .tick_rate(Duration::from_millis(16))
    .build()
    .run()
    .await;
    let result = result.map_err(|error| anyhow::anyhow!("{error}"));
    if let Some(session_id) = agent.session_id().await {
        println!("\nSession saved. Resume it with:\n\nALAN_SESSION={session_id} alan");
    }
    result
}

fn settings_path() -> anyhow::Result<PathBuf> {
    Ok(alan_data_dir()?.join("settings.json"))
}

fn build_env_patch_settings(persisted_model: Option<String>) -> anyhow::Result<PatchSettings> {
    let mut patch = PatchSettings::default();
    if let Some(model) = std::env::var_os("ALAN_MODEL") {
        patch.model = Some(model.to_string_lossy().into_owned());
    }
    if std::env::var_os("ALAN_OPENROUTER_WEB_FETCH").is_some() {
        patch.web_fetch = Some(parse_bool_env("ALAN_OPENROUTER_WEB_FETCH")?);
    }
    if std::env::var_os("ALAN_OPENROUTER_WEB_SEARCH").is_some() {
        patch.web_search = Some(parse_bool_env("ALAN_OPENROUTER_WEB_SEARCH")?);
    }
    if let Some(effort) = std::env::var_os("ALAN_REASONING_EFFORT") {
        patch.reasoning = Some(parse_reasoning_effort(&effort)?);
    }
    if let Some(order) = std::env::var_os("ALAN_OR_MODEL_PROVIDER") {
        // Provider order is scoped per model: the env override applies to the
        // model it selects (ALAN_MODEL), or the persisted one otherwise.
        let model = patch
            .model
            .clone()
            .or(persisted_model)
            .unwrap_or_else(|| DEFAULT_MODEL.into());
        let mut orders = std::collections::BTreeMap::new();
        orders.insert(model, parse_provider_order(&order)?);
        patch.provider_orders = Some(orders);
    }
    if let Some(provider) = std::env::var_os("ALAN_PROVIDER") {
        patch.provider = Some(provider.to_string_lossy().into_owned());
    }
    Ok(patch)
}

fn parse_reasoning_effort(value: &std::ffi::OsStr) -> anyhow::Result<ReasoningEffort> {
    SlashCommand::parse_effort(value.to_string_lossy().as_ref()).ok_or_else(|| {
        anyhow::anyhow!(
            "ALAN_REASONING_EFFORT must be one of none, minimal, low, medium, high, xhigh, max; got {:?}",
            value.to_string_lossy()
        )
    })
}

fn parse_provider_order(value: &std::ffi::OsStr) -> anyhow::Result<Vec<String>> {
    let order = value
        .to_string_lossy()
        .split(',')
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if order.is_empty() {
        return Err(anyhow::anyhow!("ALAN_OR_MODEL_PROVIDER must not be empty"));
    }
    Ok(order)
}

fn enabled_server_tools(
    provider: &dyn Provider,
    settings: &Settings,
) -> anyhow::Result<Vec<ServerTool>> {
    let mut enabled = Vec::new();
    for tool in provider.server_tools() {
        let enabled_for_tool = match tool.id.as_str() {
            "openrouter:web_fetch" => settings.web_fetch,
            "openrouter:web_search" => settings.web_search,
            _ => continue,
        };
        if enabled_for_tool == Some(true) {
            enabled.push(ServerTool {
                kind: tool.id.clone(),
            });
        }
    }
    Ok(enabled)
}

fn parse_bool_env(name: &str) -> anyhow::Result<bool> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(false);
    };
    match value.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        value => Err(anyhow::anyhow!(
            "{name} must be a boolean (true/false), got {value:?}"
        )),
    }
}

fn auth_path() -> anyhow::Result<PathBuf> {
    Ok(alan_data_dir()?.join("auth.json"))
}

fn sessions_path() -> anyhow::Result<PathBuf> {
    Ok(alan_data_dir()?.join("sessions"))
}

fn alan_data_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine Alan home directory"))?;
    Ok(PathBuf::from(home).join(".alan"))
}

fn configured_session_id() -> anyhow::Result<Option<String>> {
    let Some(id) = std::env::var_os("ALAN_SESSION") else {
        return Ok(None);
    };
    let id = id.to_string_lossy().trim().to_owned();
    if id.is_empty() {
        return Err(anyhow::anyhow!("ALAN_SESSION must not be empty"));
    }
    Ok(Some(id))
}
