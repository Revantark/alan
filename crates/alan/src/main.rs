mod core;
mod keymap;
mod logging;
mod login_overlay;
mod provider_overlay;
mod root;
mod views;

use crate::core::custom_providers::{self, CustomProviderStore, CustomProviders};
use crate::core::permissions::AlanPermissionManager;
use crate::core::permissions::ToolPolicy;
use crate::core::permissions_store;
use crate::core::server_tools::server_tools;
use crate::core::settings::patch_from_profile;
use crate::core::settings::{
    DEFAULT_MODEL, DEFAULT_PROVIDER, PatchSettings, Settings, SettingsStore,
};
use crate::core::update::current_version;
use crate::core::{ChatController, SlashCommand};
use crate::keymap::AlanKeyMapper;
use crate::logging::init;
use crate::root::AlanRoot;
use agent::{
    Agent, AllowAllPermissionManager, SessionManager, ToolPermissionManager, default_tools,
};
use alan_tui::Runtime;
use llm::ReasoningEffort;
use providers::{
    ChatGptProvider, DeepSeekProvider, FileCredentialStore, ModelOptions, OpenRouterProvider,
    Provider, ProviderId, ProviderRegistry, ZaiProvider, bind_model,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let is_blank = has_flag("--blank");
    let is_save = has_flag("--save");

    if has_flag("--version") {
        println!("alan-init {}", current_version());
        return Ok(());
    }

    let _guard = init()?;

    let store = SettingsStore::new(settings_path()?);
    let persisted = load_or_create_settings(&store).await?;
    let mut settings = persisted;

    let persisted_model = settings.model.clone();
    settings.apply_patch(build_env_patch_settings(persisted_model)?);

    if is_save {
        store.save(&settings).await?;
    }

    let profile_name = std::env::var_os("ALAN_PROFILE")
        .or_else(|| settings.active_profile.as_deref().map(Into::into));

    if let Some(name) = profile_name {
        let name = name.to_string_lossy().trim().to_owned();
        if !name.is_empty() {
            settings.apply_patch(patch_from_profile(&name).await?);
            settings.active_profile = Some(name);
        }
    }

    let credential_store = Arc::new(FileCredentialStore::new(auth_path()?));
    let credential_store_for_providers =
        credential_store.clone() as Arc<dyn providers::CredentialStore>;
    let providers = build_providers(&credential_store_for_providers)?;
    let registry = Arc::new(ProviderRegistry::new(providers));

    let custom_providers = CustomProviders::new(
        CustomProviderStore::new(custom_providers::default_providers_path()?),
        Arc::clone(&registry),
        Arc::clone(&credential_store_for_providers),
    );
    let startup_notices = custom_providers.load().await.unwrap_or_else(|error| {
        tracing::warn!(%error, "failed to load custom providers");
        Vec::new()
    });
    let session_manager = Arc::new(SessionManager::new(sessions_path()?));
    let resumed_session = load_resumed_session(&session_manager).await?;
    let model = select_model(&registry, &settings, resumed_session.as_ref())?;

    let current_dir = std::env::current_dir()?;

    if let Some(prompt) = parse_prompt_flag()? {
        let agent = Arc::new(build_agent(
            model,
            current_dir,
            session_manager,
            resumed_session,
            is_blank,
            Arc::new(AllowAllPermissionManager),
        )?);
        let stream = agent.ask(agent.prompt().content(prompt.trim().to_owned()))?;

        println!("{}", stream.into_response().await?.text());

        if let Some(session_id) = agent.session_id().await {
            println!("\nSession saved. Resume it with:\n\nALAN_SESSION={session_id} alan");
        }

        return Ok(());
    }

    let alan_home = core::paths::alan_home()?;
    let skill_roots = vec![
        core::skills::project_root(&current_dir),
        core::skills::personal_root(&core::paths::alan_data_dir()?),
        core::skills::shared_root(&alan_home),
    ];

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
        Arc::new(permission_manager),
    )?;

    let model_name = agent.info().await.name;
    let mut controller = ChatController::new(agent, model_name);

    if was_resumed {
        controller.restore_session_history().await;
    }
    for notice in startup_notices {
        controller.push_info(notice);
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
        skill_roots,
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

/// The `-p`/`-p=<text>` prompt flag, if present.
fn parse_prompt_flag() -> anyhow::Result<Option<String>> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-p" {
            let value = args.next().ok_or_else(|| {
                anyhow::anyhow!("-p requires a prompt, e.g. alan -p \"fix the build\"")
            })?;
            return ensure_non_empty_prompt(value).map(Some);
        }
        let Some(value) = arg.strip_prefix("-p=") else {
            continue;
        };
        return ensure_non_empty_prompt(value.to_owned()).map(Some);
    }
    Ok(None)
}

fn ensure_non_empty_prompt(value: String) -> anyhow::Result<String> {
    if value.trim().is_empty() {
        return Err(anyhow::anyhow!("-p requires a non-empty prompt"));
    }
    Ok(value)
}

async fn load_or_create_settings(store: &SettingsStore) -> anyhow::Result<Settings> {
    match store.load().await? {
        Some(settings) => Ok(settings),
        None => {
            let settings = Settings::with_defaults();
            store.save(&settings).await?;
            Ok(settings)
        }
    }
}

fn build_providers(
    credential_store: &Arc<dyn providers::CredentialStore>,
) -> anyhow::Result<Vec<Arc<dyn Provider>>> {
    Ok(vec![
        Arc::new(ZaiProvider::from_store(Arc::clone(credential_store)).build()?),
        // until there really exists some evidence that this is needed, we can add back
        // Arc::new(GoogleProvider::from_store(Arc::clone(credential_store)).build()?),
        Arc::new(OpenRouterProvider::from_store(Arc::clone(credential_store)).build()?),
        Arc::new(DeepSeekProvider::from_store(Arc::clone(credential_store)).build()?),
        Arc::new(ChatGptProvider::from_store(Arc::clone(credential_store)).build()),
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
    registry: &ProviderRegistry,
    settings: &Settings,
    resumed_session: Option<&agent::Session>,
) -> anyhow::Result<providers::Model> {
    let (provider_id, model_id) = match resumed_session {
        Some(session) => (session.provider.as_str(), session.model.clone()),
        None => (
            settings.provider.as_deref().unwrap_or(DEFAULT_PROVIDER),
            settings
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.into()),
        ),
    };
    let provider = registry
        .get(&ProviderId::new(provider_id))
        .ok_or_else(|| anyhow::anyhow!("Provider not found: {provider_id}"))?;
    let options = ModelOptions {
        server_tools: server_tools(
            provider.server_tools(),
            settings.web_fetch == Some(true),
            settings.web_search == Some(true),
        ),
        reasoning_effort: settings.reasoning.unwrap_or_default(),
        extensions: settings.extensions(&model_id),
    };
    Ok(bind_model(provider.as_ref(), &model_id, options)?)
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
    permission_manager: Arc<dyn ToolPermissionManager>,
) -> anyhow::Result<Agent> {
    let mut builder = Agent::builder(model)
        .with_directory(current_dir)
        .with_tools(default_tools())
        .session_manager(session_manager)
        .permission_manager(permission_manager);
    if !is_blank {
        builder = builder.with_default_system_prompt();
    }
    if let Some(session) = resumed_session {
        builder = builder.resume_session(session);
    }
    Ok(builder.build()?)
}

fn settings_path() -> anyhow::Result<PathBuf> {
    core::settings::default_settings_path()
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
    Ok(core::paths::alan_data_dir()?.join("auth.json"))
}

fn sessions_path() -> anyhow::Result<PathBuf> {
    Ok(core::paths::alan_data_dir()?.join("sessions"))
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
