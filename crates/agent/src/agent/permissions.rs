use async_trait::async_trait;
use llm::ToolCall;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

#[async_trait]
pub trait ToolPermissionManager: Send + Sync {
    async fn has_permission(&self, call: &ToolCall) -> PermissionDecision;
    async fn request_permission(&self, call: &ToolCall) -> PermissionDecision;
}

pub struct AllowAllPermissionManager;

#[async_trait]
impl ToolPermissionManager for AllowAllPermissionManager {
    async fn has_permission(&self, _call: &ToolCall) -> PermissionDecision {
        PermissionDecision::Allow
    }
    async fn request_permission(&self, _call: &ToolCall) -> PermissionDecision {
        PermissionDecision::Allow
    }
}
