use agent::{Permission, ToolPermissionManager};
use async_trait::async_trait;
use llm::ToolCall;

pub struct AlanPermissionManager;

#[async_trait]
impl ToolPermissionManager for AlanPermissionManager {
    async fn authorize(&self, _call: &ToolCall) -> Permission {
        Permission::Allowed
    }
}
