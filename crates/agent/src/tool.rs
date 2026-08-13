use llm::ToolDefinition;
use std::sync::Arc;
use tools::{
    BashExecutor, FileEditExecutor, FileReadExecutor, FileWriteExecutor, ToolExecutor,
    bash_definition, file_edit_definition, file_read_definition, file_write_definition,
};

pub struct AgentTool {
    pub definition: ToolDefinition,
    pub executor: Arc<dyn ToolExecutor>,
}

impl AgentTool {
    pub fn new(definition: ToolDefinition, executor: impl ToolExecutor + 'static) -> Self {
        Self {
            definition,
            executor: Arc::new(executor),
        }
    }
}

pub fn default_tools() -> Vec<AgentTool> {
    vec![
        AgentTool::new(file_read_definition(), FileReadExecutor),
        AgentTool::new(file_write_definition(), FileWriteExecutor),
        AgentTool::new(file_edit_definition(), FileEditExecutor),
        AgentTool::new(bash_definition(), BashExecutor),
    ]
}
