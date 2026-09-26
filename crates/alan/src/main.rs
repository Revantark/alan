mod core;
mod local_model_overlay;
mod local_model_store;
mod logging;
mod login_overlay;
mod root;
mod views;

use crate::core::permissions::AlanPermissionManager;
use crate::core::permissions::ToolPolicy;
use crate::core::permissions_store;
use crate::core::settings::{DEFAULT_MODEL, PatchSettings, Settings, SettingsStore};
use crate::core::{ChatController, SlashCommand};
use crate::local_model_store::JsonLocalModelStore;
use crate::logging::init;
use crate::root::{AlanKeyMapper, AlanRoot};
use agent::{Agent, SessionManager, default_tools};
use alan_tui::Runtime;
use llm::{ReasoningEffort, ServerTool};
use providers::{
    FileCredentialStore, GoogleProvider, LocalProvider, ModelOptions, OpenRouterProvider, Provider,
    ProviderRegistry, ZaiProvider, bind_model,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let is_blank = has_flag("--blank");
    let is_save = has_flag("--save");

    if has_flag("--version") {
        println!("alan-init {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let _guard = init()?;

    let store = SettingsStore::<Settings>::new(settings_path()?);
    let persisted = load_or_create_settings(&store).await?;
    let persisted_model = persisted.model.clone();
    let mut settings = persisted;
    settings.apply_patch(build_env_patch_settings(persisted_model)?);

    if is_save {
        store.save(&settings).await?;
    }

    let credential_store = Arc::new(FileCredentialStore::new(auth_path()?));
    let credential_store_for_providers =
        credential_store.clone() as Arc<dyn providers::CredentialStore>;
    let local_provider = load_local_provider().await?;
    let providers = build_providers(&credential_store_for_providers, &local_provider)?;
    let session_manager = Arc::new(SessionManager::new(sessions_path()?));
    let resumed_session = load_resumed_session(&session_manager).await?;
    let model = select_model(&providers, &settings, resumed_session.as_ref())?;
    let registry = Arc::new(ProviderRegistry::with_local_provider(
        providers,
        Arc::clone(&local_provider),
    ));

    let current_dir = std::env::current_dir()?;
    let policy = build_tool_policy(&settings, &current_dir)?;
    let permission_manager = AlanPermissionManager::init(policy.clone());
    let permission_handler = permission_manager.handler();
    let was_resumed = resumed_session.is_some();

    let agent = build_agent(
        model,
        current_dir,
        session_manager,
        resumed_session,
        is_blank,
    )?;

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
        permission_handler,
        policy,
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

fn has_flag(flag: &str) -> bool {
    std::env::args().any(|arg| arg == flag)
}

async fn load_or_create_settings(store: &SettingsStore<Settings>) -> anyhow::Result<Settings> {
    match store.load().await? {
        Some(settings) => Ok(settings),
        None => {
            let settings = Settings::with_defaults();
            store.save(&settings).await?;
            Ok(settings)
        }
    }
}

async fn load_local_provider() -> anyhow::Result<Arc<LocalProvider>> {
    let store = Arc::new(JsonLocalModelStore::new(
        alan_data_dir()?.join("local_models.json"),
    ));
    let provider = Arc::new(LocalProvider::new(store));
    if let Err(error) = provider.load().await {
        tracing::warn!("failed to load local models: {error}");
    }
    Ok(provider)
}

fn build_providers(
    credential_store: &Arc<dyn providers::CredentialStore>,
    local_provider: &Arc<LocalProvider>,
) -> anyhow::Result<Vec<Arc<dyn Provider>>> {
    Ok(vec![
        Arc::new(ZaiProvider::from_store(Arc::clone(credential_store)).build()?),
        Arc::new(GoogleProvider::from_store(Arc::clone(credential_store)).build()?),
        Arc::new(OpenRouterProvider::from_store(Arc::clone(credential_store)).build()?),
        Arc::clone(local_provider) as Arc<dyn Provider>,
    ])
}

async fn load_resumed_session(
    session_manager: &SessionManager,
) -> anyhow::Result<Option<agent::Session>> {
    let Some(session_id) = configured_session_id()? else {
        return Ok(None);
    };
    let cwd = std::env::current_dir()?;
    Ok(Some(session_manager.get_session(&session_id, &cwd).await?))
}

fn select_model(
    providers: &[Arc<dyn Provider>],
    settings: &Settings,
    resumed_session: Option<&agent::Session>,
) -> anyhow::Result<providers::Model> {
    let (provider_id, model_id) = match resumed_session {
        Some(session) => (session.provider.as_str(), session.model.clone()),
        None => (
            settings.provider.as_deref().unwrap_or("openrouter"),
            settings
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.into()),
        ),
    };
    let provider = providers
        .iter()
        .find(|provider| provider.id().0 == provider_id)
        .ok_or_else(|| anyhow::anyhow!("Provider not found: {provider_id}"))?;
    let server_tools = enabled_server_tools(provider.as_ref(), settings)?;

    Ok(bind_model(
        provider.as_ref(),
        &model_id,
        ModelOptions {
            server_tools,
            reasoning_effort: settings.reasoning.unwrap_or_default(),
            provider_order: settings.provider_order(&model_id),
        },
    )?)
}

fn build_tool_policy(
    settings: &Settings,
    current_dir: &std::path::Path,
) -> anyhow::Result<ToolPolicy> {
    let permissions_path = permissions_store::default_permissions_path(current_dir)
        .expect("cannot determine permissions path (set ALAN_HOME or HOME)");
    let policy = ToolPolicy::new(Arc::new(permissions_store::PermissionStore::new(
        permissions_path,
    )));
    if let Some(saved) = settings.tool_policy {
        policy.set_policy(saved);
    }
    Ok(policy)
}

fn build_agent(
    model: providers::Model,
    current_dir: PathBuf,
    session_manager: Arc<SessionManager>,
    resumed_session: Option<agent::Session>,
    is_blank: bool,
) -> anyhow::Result<Agent> {
    let mut builder = Agent::builder(model)
        .with_directory(current_dir)
        .with_tools(default_tools())
        .session_manager(session_manager);
    if !is_blank {
        builder = builder.with_default_system_prompt();
    }
    if let Some(session) = resumed_session {
        builder = builder.resume_session(session);
    }
    Ok(builder.build()?)
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
