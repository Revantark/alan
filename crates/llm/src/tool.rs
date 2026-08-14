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

/// Tool call issued by model. Arguments are raw JSON from wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}
