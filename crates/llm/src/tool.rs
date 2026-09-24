use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A provider-managed tool. The provider, rather than Alan, executes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerTool {
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolSpec {
    Function(ToolDefinition),
    Server(ServerTool),
}

impl From<ToolDefinition> for ToolSpec {
    fn from(tool: ToolDefinition) -> Self {
        Self::Function(tool)
    }
}

/// The category of a tool call based on its expected side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads data without modifying external state.
    Read,
    /// Modifies persistent state without executing an arbitrary command.
    Write,
    /// Performs network communication without executing the received or remote data.
    Network,
    /// The tool call could not be reliably classified.
    #[default]
    Unknown,
}

/// Tool call issued by model. Arguments are raw JSON from wire protocol.
///
/// This is pure wire data. Side-effect classification (`ToolKind`) is not
/// part of the call: tools with a statically known kind (read/write/edit)
/// are classified from the tool itself, while `bash` carries `kind` inside
/// its arguments and the agent resolves it before authorization.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// Thought signature from thinking models (e.g. Gemini 3+ Interactions
    /// API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}
