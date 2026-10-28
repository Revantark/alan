use tools::{
    BashExecutor, FileEditExecutor, FileReadExecutor, FileWriteExecutor, bash_definition,
    file_edit_definition, file_read_definition, file_write_definition,
};

/// A tool the agent carries.
///
/// This is the unified [`tools::ProviderTool`]: local tools the agent runs and
/// server tools the provider runs share one type. See that type for the
/// server/local distinction.
pub use tools::ProviderTool as AgentTool;

pub fn default_tools() -> Vec<AgentTool> {
    vec![
        AgentTool::new(file_read_definition(), FileReadExecutor).read_only(),
        AgentTool::new(file_write_definition(), FileWriteExecutor),
        AgentTool::new(file_edit_definition(), FileEditExecutor),
        AgentTool::new(bash_definition(), BashExecutor),
    ]
}
