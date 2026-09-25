//! Tool-permission gating for the interactive UI.
//!
//! Prompting is gated by a [`ToolPolicy`]: a mode (Free/Slip/Strict) plus
//! the grants the user has approved so far. The permission actor consults
//! it before showing a request; the UI updates it via `/tfree`, `/tslip`,
//! and `/tstrict`.

use agent::{Permission, ToolPermissionManager};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use llm::ToolCall;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Tool-permission mode. Determines which grants [`ToolPolicy`] consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// Allow every tool call without asking.
    Free,
    /// Allow any command whose family (leading command word) was approved.
    Slip,
    /// Allow only the exact command (tool + arguments) already approved.
    #[default]
    Strict,
}

impl Display for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Free => "free",
            Self::Slip => "slip",
            Self::Strict => "strict",
        };

        f.write_str(name)
    }
}

/// For shell-style calls (tool name `"bash"`), the command family is the
/// first word of the arguments (e.g. `bun` from `bun dev --host`).
/// For other tools, the tool name itself is the command.
fn parse_command(name: &str, arguments: &str) -> (String, Option<String>) {
    let args = arguments.split_whitespace().collect::<Vec<_>>().join(" ");

    if name != "bash" {
        return (name.to_owned(), Some(args));
    }
    let Some((command, rest)) = args.split_once(' ') else {
        return (args, None);
    };

    (command.to_owned(), Some(rest.to_owned()))
}

/// An approved tool call. `args: None` is a family grant (any arguments for
/// `command`); `Some` is an exact grant for those arguments only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Grant {
    pub command: String,
    pub args: Option<String>,
}

impl Grant {
    pub fn of(call: &ToolCall) -> Self {
        let parsed = parse_command(&call.name, &call.arguments);
        Self {
            command: parsed.0,
            args: parsed.1,
        }
    }

    /// The family grant this one would promote to under Slip mode.
    fn family(&self) -> Self {
        Self {
            command: self.command.clone(),
            args: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
}

/// Pure policy rules over a mode and a grant set. No locks, no async.
#[derive(Debug, Default)]
pub struct PolicyState {
    pub policy: Policy,
    pub grants: HashSet<Grant>,
}

impl PolicyState {
    fn decide(&self, call: &ToolCall) -> Decision {
        match self.policy {
            Policy::Free => Decision::Allow,
            Policy::Strict => {
                if self.grants.contains(&Grant::of(call)) {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
            Policy::Slip => {
                let grant = Grant::of(call);
                if self.grants.contains(&grant) {
                    return Decision::Allow;
                }
                if self.grants.contains(&grant.family()) {
                    return Decision::Allow;
                }
                Decision::Ask
            }
        }
    }

    fn promote(&mut self, grant: Grant) {
        self.grants.insert(grant.family());
        self.grants.insert(grant);
    }
}

/// The policy rules plus their shared state. A thin `Arc<Mutex<...>>` over
/// [`PolicyState`]; the permission actor and the UI both hold a clone.
#[derive(Debug, Clone, Default)]
pub struct ToolPolicy(Arc<Mutex<PolicyState>>);

impl ToolPolicy {
    /// Consult the policy, applying any promotion it prescribes.
    pub fn check(&self, call: &ToolCall) -> Decision {
        let state = self.0.lock().expect("policy lock");
        state.decide(call)
    }

    pub fn set_policy(&self, policy: Policy) {
        self.0.lock().expect("policy lock").policy = policy;
    }
}

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
    policy: ToolPolicy,
    handler: PermissionHandler,
}

impl AlanPermissionManager {
    pub fn init(policy: ToolPolicy) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(16);
        let (request_tx, _) = tokio::sync::broadcast::channel::<PermissionRequest>(16);
        tokio::spawn(listen(
            cmd_rx,
            request_tx.clone(),
            Arc::new(Mutex::new(HashMap::new())),
            policy.clone(),
        ));
        Self {
            tx: cmd_tx.clone(),
            policy,
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
        match self.policy.check(call) {
            Decision::Allow => return Permission::Allowed,
            Decision::Ask => {}
        }
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
    pending: Arc<Mutex<HashMap<u64, (oneshot::Sender<Permission>, Grant)>>>,
    policy: ToolPolicy,
) {
    while let Some(command) = cmd_rx.recv().await {
        match command {
            Command::Request { request, respond } => {
                let id = request.id;
                let (command, args) = parse_command(&request.name, &request.arguments);
                let grant = Grant { command, args };

                if request_tx.send(request).is_err() {
                    drop(respond);
                } else {
                    pending
                        .lock()
                        .expect("pending lock")
                        .insert(id, (respond, grant));
                }
            }
            Command::Respond { id, decision } => {
                let Some((respond, grant)) = pending.lock().expect("pending lock").remove(&id)
                else {
                    continue;
                };
                let _ = respond.send(decision.clone());
                if decision == Permission::Allowed {
                    let mut state = policy.0.lock().expect("policy lock");
                    state.promote(grant);
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
    use std::time::Duration;

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

    /// A shell-style call: the family is the first word of the arguments.
    fn shell(arguments: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: arguments.into(),
            signature: None,
        }
    }

    #[tokio::test]
    async fn respond_unblocks_authorize() {
        let manager = AlanPermissionManager::init(ToolPolicy::default());
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id, Permission::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }

    #[tokio::test]
    async fn strict_allows_after_approval() {
        let policy = ToolPolicy::default();
        let manager1 = AlanPermissionManager::init(policy.clone());
        let handler1 = manager1.handler();

        // First call: strict mode asks, user approves.
        let handle = tokio::spawn(async move { manager1.authorize(&shell("bun dev")).await });
        let request = handler1.subscribe().next().await.expect("request");
        handler1.respond(request.id, Permission::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);

        // Second manager shares the same policy state; same call should be allowed.
        let manager2 = AlanPermissionManager::init(policy);
        let handler2 = manager2.handler();
        let handle = tokio::spawn(async move { manager2.authorize(&shell("bun dev")).await });
        // No request should appear — the exact grant was recorded.
        let mut rx = handler2.subscribe();
        tokio::select! {
            _ = rx.next() => panic!("strict mode should not prompt for an already-approved exact command"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        drop(handle);
    }

    #[tokio::test]
    async fn denied_without_ui() {
        let manager = AlanPermissionManager::init(ToolPolicy::default());
        // Never subscribe: the actor finds no listeners and drops the
        // responder, so authorize denies instead of hanging.
        assert_eq!(manager.authorize(&call("bash")).await, Permission::Denied);
    }

    #[tokio::test]
    async fn stale_respond_is_ignored() {
        let manager = AlanPermissionManager::init(ToolPolicy::default());
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id + 999, Permission::Denied);
        handler.respond(request.id, Permission::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }

    #[tokio::test]
    async fn free_policy_allows_without_prompting() {
        let policy = ToolPolicy::default();
        policy.set_policy(Policy::Free);
        let manager = AlanPermissionManager::init(policy);
        assert_eq!(
            manager.authorize(&shell("bun dev --host")).await,
            Permission::Allowed
        );
    }

    fn state_of(policy: Policy, grants: &[(&str, Option<&str>)]) -> PolicyState {
        PolicyState {
            policy,
            grants: grants
                .iter()
                .map(|(command, args)| Grant {
                    command: (*command).into(),
                    args: args.map(|args| args.into()),
                })
                .collect(),
        }
    }

    #[test]
    fn strict_allows_only_exact_grants() {
        let state = state_of(Policy::Strict, &[("bun", Some("dev")), ("cargo", None)]);
        assert_eq!(state.decide(&shell("bun dev")), Decision::Allow);
        assert_eq!(state.decide(&shell("bun dev --host")), Decision::Ask);
        // A family grant is not consulted in strict mode.
        assert_eq!(state.decide(&shell("cargo test")), Decision::Ask);
        assert_eq!(state.decide(&shell("node -v")), Decision::Ask);
    }

    #[test]
    fn grant_of_normalizes_whitespace_and_splits_family() {
        let grant = Grant::of(&shell("  bun \t dev   --host "));
        assert_eq!(
            grant,
            Grant {
                command: "bun".into(),
                args: Some("dev --host".into())
            }
        );
        // Single-word arguments: the whole string is the family.
        assert_eq!(
            Grant::of(&shell("bun")),
            Grant {
                command: "bun".into(),
                args: None
            }
        );
    }
}
