//! Tool-permission gating for the interactive UI.

use agent::{Permission, ToolPermissionManager};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use llm::ToolCall;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// A pending tool-authorization request shown to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub id: u64,
    pub name: String,
    pub arguments: String,
}

enum Command {
    Request {
        request: PermissionRequest,
        respond: oneshot::Sender<Permission>,
    },
    Respond {
        id: u64,
        decision: Permission,
    },
}

pub struct AlanPermissionManager {
    tx: mpsc::Sender<Command>,
    handler: PermissionHandler,
}

impl AlanPermissionManager {
    pub fn init() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(16);
        let (request_tx, _) = tokio::sync::broadcast::channel::<PermissionRequest>(16);
        tokio::spawn(listen(
            cmd_rx,
            request_tx.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        ));
        Self {
            tx: cmd_tx.clone(),
            handler: PermissionHandler {
                requests: request_tx,
                commands: cmd_tx,
            },
        }
    }

    pub fn handler(&self) -> PermissionHandler {
        self.handler.clone()
    }
}

#[async_trait]
impl ToolPermissionManager for AlanPermissionManager {
    async fn authorize(&self, call: &ToolCall) -> Permission {
        let request = PermissionRequest {
            id: next_id(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        };
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(Command::Request {
                request,
                respond: tx,
            })
            .await
            .is_err()
        {
            return Permission::Denied;
        }
        rx.await.unwrap_or(Permission::Denied)
    }
}

#[derive(Clone)]
pub struct PermissionHandler {
    requests: tokio::sync::broadcast::Sender<PermissionRequest>,
    commands: mpsc::Sender<Command>,
}

impl PermissionHandler {
    /// Stream of pending permission requests.
    pub fn subscribe(&self) -> impl Stream<Item = PermissionRequest> + Send + 'static {
        let rx = Some(self.requests.subscribe());
        futures_util::stream::unfold(rx, move |mut rx| async move {
            let mut rx = rx.take()?;
            loop {
                match rx.recv().await {
                    Ok(request) => return Some((request, Some(rx))),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }

    /// Answer a pending request; ignored if it was already answered.
    pub fn respond(&self, id: u64, decision: Permission) {
        let _ = self.commands.try_send(Command::Respond { id, decision });
    }
}

async fn listen(
    mut cmd_rx: mpsc::Receiver<Command>,
    request_tx: tokio::sync::broadcast::Sender<PermissionRequest>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Permission>>>>,
) {
    while let Some(command) = cmd_rx.recv().await {
        match command {
            Command::Request { request, respond } => {
                let id = request.id;
                if request_tx.send(request).is_err() {
                    drop(respond);
                } else {
                    pending.lock().expect("pending lock").insert(id, respond);
                }
            }
            Command::Respond { id, decision } => {
                if let Some(respond) = pending.lock().expect("pending lock").remove(&id) {
                    let _ = respond.send(decision);
                }
            }
        }
    }
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::ToolCall;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: name.into(),
            arguments: "{}".into(),
            signature: None,
        }
    }

    #[tokio::test]
    async fn respond_unblocks_authorize() {
        let manager = AlanPermissionManager::init();
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id, Permission::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }

    #[tokio::test]
    async fn denied_without_ui() {
        let manager = AlanPermissionManager::init();
        // Never subscribe: the actor finds no listeners and drops the
        // responder, so authorize denies instead of hanging.
        assert_eq!(manager.authorize(&call("bash")).await, Permission::Denied);
    }

    #[tokio::test]
    async fn stale_respond_is_ignored() {
        let manager = AlanPermissionManager::init();
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id + 999, Permission::Denied);
        handler.respond(request.id, Permission::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }
}
