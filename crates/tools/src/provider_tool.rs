//! A provider's tool catalog entry.
//!
//! Providers advertise tools with [`ProviderTool`]. The two execution models
//! share one shape: a *server* tool is declared to the model and run by the
//! provider's remote service, while a *local* tool is run by the agent itself.
//! The only structural difference is the JSON schema — present for function
//! tools, absent for server tools — which [`ProviderTool::spec`] turns into the
//! right wire item.

use crate::tool::{ToolError, ToolExecutor, ToolOutput};
use async_trait::async_trait;
use llm::{ServerTool, ToolCall, ToolDefinition, ToolSpec};
use serde_json::Value;
use std::sync::Arc;

/// A setting-gated capability a tool serves.
///
/// `None` (the common case) means the tool is always available. A `Some`
/// capability lets the application map a tool to the user setting that turns it
/// on, without the provider having to know anything about that setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    WebFetch,
    WebSearch,
}

/// A tool a provider offers, either provider- or agent-executed.
#[derive(Clone)]
pub struct ProviderTool {
    id: String,
    description: String,
    /// `Some` for a function tool (JSON schema), `None` for a server tool.
    parameters: Option<Value>,
    executor: Arc<dyn ToolExecutor>,
    read_only: bool,
    capability: Option<Capability>,
}

impl ProviderTool {
    /// A function tool with a JSON schema, executed by `executor`.
    pub fn new(definition: ToolDefinition, executor: impl ToolExecutor + 'static) -> Self {
        Self {
            id: definition.name,
            description: definition.description,
            parameters: Some(definition.parameters),
            executor: Arc::new(executor),
            read_only: false,
            capability: None,
        }
    }

    /// A provider-executed tool: declared to the model, never run by the agent.
    pub fn server(id: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            parameters: None,
            executor: Arc::new(ServerToolExecutor),
            read_only: true,
            capability: None,
        }
    }

    /// Mark the tool as free of side effects.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Attach the setting-gated capability this tool serves.
    pub fn with_capability(mut self, capability: Capability) -> Self {
        self.capability = Some(capability);
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn capability(&self) -> Option<Capability> {
        self.capability
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn executor(&self) -> Arc<dyn ToolExecutor> {
        self.executor.clone()
    }

    /// The wire form: a function tool when it carries a schema, otherwise a
    /// provider-executed server tool.
    pub fn spec(&self) -> ToolSpec {
        match &self.parameters {
            Some(parameters) => ToolSpec::Function(ToolDefinition {
                name: self.id.clone(),
                description: self.description.clone(),
                parameters: parameters.clone(),
            }),
            None => ToolSpec::Server(ServerTool {
                kind: self.id.clone(),
            }),
        }
    }
}

/// Executor backing server tools.
///
/// A server tool's call is answered by the provider and never arrives at the
/// agent. The agent intentionally assumes this; if the assumption is ever
/// violated we fail loudly instead of silently returning an empty result, so
/// the bug surfaces instead of corrupting the conversation.
pub(crate) struct ServerToolExecutor;

#[async_trait]
impl ToolExecutor for ServerToolExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        Err(ToolError(format!(
            "server tool {:?} is executed by the provider, not the agent",
            call.name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolExecutor;
    use serde_json::json;

    struct OkExecutor;

    #[async_trait]
    impl ToolExecutor for OkExecutor {
        async fn execute(&self, _call: &ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::Text("ok".into()))
        }
    }

    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: "desc".into(),
            parameters: json!({"type": "object"}),
        }
    }

    #[test]
    fn function_tool_serializes_with_its_schema() {
        let tool = ProviderTool::new(definition("read"), OkExecutor);
        assert_eq!(tool.id(), "read");
        assert_eq!(tool.description(), "desc");
        assert!(!tool.is_read_only());
        assert_eq!(tool.capability(), None);
        assert!(matches!(tool.spec(), ToolSpec::Function(_)));
        match tool.spec() {
            ToolSpec::Function(def) => {
                assert_eq!(def.name, "read");
                assert_eq!(def.parameters, json!({"type": "object"}));
            }
            ToolSpec::Server(_) => panic!("function tool must not serialize as server"),
        }
    }

    #[test]
    fn read_only_and_capability_are_recorded() {
        let tool = ProviderTool::new(definition("web_search"), OkExecutor)
            .read_only()
            .with_capability(Capability::WebSearch);
        assert!(tool.is_read_only());
        assert_eq!(tool.capability(), Some(Capability::WebSearch));
    }

    #[test]
    fn server_tool_serializes_as_a_typed_item_without_a_schema() {
        let tool = ProviderTool::server("openrouter:web_fetch", "Fetch a web page")
            .with_capability(Capability::WebFetch);
        assert_eq!(tool.id(), "openrouter:web_fetch");
        assert_eq!(tool.description(), "Fetch a web page");
        assert!(tool.is_read_only());
        assert_eq!(tool.capability(), Some(Capability::WebFetch));
        match tool.spec() {
            ToolSpec::Server(server) => assert_eq!(server.kind, "openrouter:web_fetch"),
            ToolSpec::Function(_) => panic!("server tool must not serialize as a function"),
        }
    }

    #[tokio::test]
    async fn server_tool_executor_fails_loudly() {
        let tool = ProviderTool::server("openrouter:web_search", "Search");
        let call = ToolCall {
            name: "openrouter:web_search".into(),
            ..ToolCall::default()
        };
        let error = tool.executor().execute(&call).await.unwrap_err();
        assert!(error.to_string().contains("executed by the provider"));
    }
}
