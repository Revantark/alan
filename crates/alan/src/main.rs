mod core;
mod logging;
mod login_overlay;
mod tui_root;
mod views;

use core::Controller;
use llm::ServerTool;
use std::time::Duration;

use agent::{Agent, SessionManager, default_tools};
use llm::ReasoningEffort;
use providers::{
    FileCredentialStore, ModelOptions, OpenRouterProvider, Provider, ProviderRegistry,
};
use std::path::PathBuf;
use std::sync::Arc;
use tui::Runtime;

use crate::logging::init;
use crate::tui_root::{AlanKeyMapper, AlanRoot};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _guard = init().unwrap();
    let model_id = std::env::var("ALAN_MODEL").unwrap_or_else(|_| "openai/gpt-4o-mini".into());
    let credential_store = Arc::new(FileCredentialStore::new(auth_path()?));
    let provider = OpenRouterProvider::from_store(credential_store.clone())
        .with_model(&model_id)
        .build()?;
    let server_tools = enabled_server_tools(&provider)?;
    let reasoning_effort = configured_reasoning_effort()?;
    let model = provider.bind_with_options(
        &model_id,
        ModelOptions {
            server_tools,
            reasoning_effort,
        },
    )?;
    let registry = Arc::new(ProviderRegistry::new([
        Arc::new(provider) as Arc<dyn Provider>
    ]));
    let session_manager = Arc::new(SessionManager::new(sessions_path()?));
    let resumed_session = if let Some(session_id) = configured_session_id()? {
        let cwd = std::env::current_dir()?;
        Some(session_manager.get_session(&session_id, &cwd).await?)
    } else {
        None
    };

    let was_resumed = resumed_session.is_some();
    let current_dir = std::env::current_dir()?;
    let mut agent_builder = Agent::builder(model)
        .with_default_system_prompt()
        .with_directory(current_dir)
        .with_tools(default_tools())
        .session_manager(session_manager);
    if let Some(session) = resumed_session {
        agent_builder = agent_builder.resume_session(session);
    }
    let agent = agent_builder.build()?;

    let mut app = Controller::new(agent);
    if was_resumed {
        app.restore_session_history().await;
    }
    // `Runtime::run` consumes the root, so keep the agent for the saved-session
    // message printed after the TUI exits.
    let agent = app.agent();
    let result = Runtime::builder(AlanRoot::new(app, registry, credential_store))
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

fn enabled_server_tools(provider: &OpenRouterProvider) -> anyhow::Result<Vec<ServerTool>> {
    let mut enabled = Vec::new();
    for tool in provider.server_tools() {
        let variable = match tool.id.as_str() {
            "openrouter:web_fetch" => "ALAN_OPENROUTER_WEB_FETCH",
            "openrouter:web_search" => "ALAN_OPENROUTER_WEB_SEARCH",
            _ => continue,
        };
        if parse_bool_env(variable)? {
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

fn configured_reasoning_effort() -> anyhow::Result<Option<ReasoningEffort>> {
    let Some(value) = std::env::var_os("ALAN_REASONING_EFFORT") else {
        return Ok(None);
    };
    match value.to_string_lossy().trim().to_ascii_lowercase().as_str() {
        "none" => Ok(None),
        "minimal" => Ok(Some(ReasoningEffort::Minimal)),
        "low" => Ok(Some(ReasoningEffort::Low)),
        "medium" => Ok(Some(ReasoningEffort::Medium)),
        "high" => Ok(Some(ReasoningEffort::High)),
        "xhigh" => Ok(Some(ReasoningEffort::XHigh)),
        "max" => Ok(Some(ReasoningEffort::Max)),
        value => Err(anyhow::anyhow!(
            "ALAN_REASONING_EFFORT must be one of none, minimal, low, medium, high, xhigh, max; got {value:?}"
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
