use async_trait::async_trait;
use llm::ToolCall;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Allowed,
    Denied,
}

#[async_trait]
pub trait ToolPermissionManager: Send + Sync {
    async fn authorize(&self, call: &ToolCall) -> Permission;
}

pub struct AllowAllPermissionManager;

#[async_trait]
impl ToolPermissionManager for AllowAllPermissionManager {
    async fn authorize(&self, _call: &ToolCall) -> Permission {
        Permission::Allowed
    }
}
