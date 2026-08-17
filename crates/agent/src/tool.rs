use llm::ToolDefinition;
use std::sync::Arc;
use tools::{
    BashExecutor, FileEditExecutor, FileReadExecutor, FileWriteExecutor, ToolExecutor,
    bash_definition, file_edit_definition, file_read_definition, file_write_definition,
};

pub struct AgentTool {
    pub definition: ToolDefinition,
    pub executor: Arc<dyn ToolExecutor>,
    pub read_only: bool,
}

impl AgentTool {
    pub fn new(definition: ToolDefinition, executor: impl ToolExecutor + 'static) -> Self {
        Self {
            definition,
            executor: Arc::new(executor),
            read_only: false,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

pub fn default_tools() -> Vec<AgentTool> {
    vec![
        AgentTool::new(file_read_definition(), FileReadExecutor).read_only(),
        AgentTool::new(file_write_definition(), FileWriteExecutor),
        AgentTool::new(file_edit_definition(), FileEditExecutor),
        AgentTool::new(bash_definition(), BashExecutor),
    ]
}
