//! Tool-permission gating for the interactive UI.

use agent::{Permission, ToolPermissionManager};
use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use llm::{ToolCall, ToolKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};
use tools::parse_kind;
use tracing::warn;

use crate::core::permissions_store::PermissionStore;

/// Tool-permission mode. Determines which grants [`ToolPolicy`] consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Allow every tool call without asking.
    Free,
    /// Allow any command whose family (leading command word) was approved.
    Slip,
    /// Allow only the exact command (tool + arguments) already approved.
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Grant {
    pub command: String,
    #[serde(default)]
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

/// The user's answer to a tool-authorization prompt. Persisted only for
/// [`Answer::AllowedAlways`]; everything else is session-scoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Allow this exact call (recorded in memory, not persisted).
    Allowed,
    /// Allow this call and promote it to a family grant (session only).
    AllowedSession,
    /// Allow and persist the exact and family grants for this project.
    AllowedAlways,
    /// Deny this call; the stream continues without the tool result.
    Denied,
    /// Deny this call (session-only scope; deny is never persisted).
    DeniedSession,
    /// Abort the stream before the tool call runs.
    Stop,
}

impl Answer {
    /// Whether this answer grants the tool call.
    fn allows(&self) -> bool {
        matches!(
            self,
            Self::Allowed | Self::AllowedSession | Self::AllowedAlways
        )
    }
}

impl From<Answer> for Permission {
    fn from(answer: Answer) -> Self {
        if answer.allows() {
            Permission::Allowed
        } else {
            Permission::Denied
        }
    }
}

/// Pure policy rules over a mode and a grant set. No locks, no async.
/// `store` is the per-project persistence; consulted lazily on a miss and
/// never used inside the pure `decide`/`promote` methods.
pub struct PolicyState {
    pub policy: Policy,
    pub grants: HashSet<Grant>,
    pub store: Arc<PermissionStore>,
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
#[derive(Clone)]
pub struct ToolPolicy(Arc<Mutex<PolicyState>>);

impl ToolPolicy {
    pub fn new(store: Arc<PermissionStore>) -> Self {
        Self(Arc::new(Mutex::new(PolicyState {
            policy: Policy::Strict,
            grants: HashSet::new(),
            store,
        })))
    }

    /// Consult the policy: HashSet first, then the store lazily,
    /// backfilling any store hit into the HashSet. The lock is never held
    /// across the store read.
    pub async fn check(&self, call: &ToolCall) -> Decision {
        let store = {
            let state = self.0.lock().expect("policy lock");
            if state.decide(call) == Decision::Allow {
                return Decision::Allow;
            }
            state.store.clone()
        };
        // The lock is dropped before the store is read.
        match store.load_all().await {
            Ok(persisted) => {
                let mut state = self.0.lock().expect("policy lock");
                state.grants.extend(persisted);
                state.decide(call)
            }
            Err(error) => {
                warn!(%error, "failed to read persisted permissions");
                Decision::Ask
            }
        }
    }

    /// Answer a pending request: update the session grants, persist when the
    /// answer is [`Answer::AllowedAlways`], and translate to the agent's
    /// two-way [`Permission`].
    pub async fn answer(&self, grant: Grant, answer: Answer) -> Permission {
        if !answer.allows() {
            return answer.into();
        }

        let store = {
            let mut state = self.0.lock().expect("policy lock");
            state.promote(grant.clone());
            state.store.clone()
        };

        if answer != Answer::AllowedAlways {
            return answer.into();
        }

        if let Err(error) = store.insert(&grant).await {
            warn!(%error, "failed to persist permission grant");
        }
        if grant.args.is_some() {
            let family = grant.family();

            if let Err(error) = store.insert(&family).await {
                warn!(%error, "failed to persist permission family grant");
            }
        }

        answer.into()
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
        decision: Answer,
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
        if parse_kind(call) == ToolKind::Read {
            return Permission::Allowed;
        }

        if let Decision::Allow = self.policy.check(call).await {
            return Permission::Allowed;
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
        futures_util::stream::unfold(self.requests.subscribe(), move |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(request) => return Some((request, rx)),
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }

    /// Answer a pending request; ignored if it was already answered.
    pub fn respond(&self, id: u64, decision: Answer) {
        let _ = self.commands.try_send(Command::Respond { id, decision });
    }
}

type PendingMap = HashMap<u64, (oneshot::Sender<Permission>, Grant)>;

async fn listen(
    mut cmd_rx: mpsc::Receiver<Command>,
    request_tx: broadcast::Sender<PermissionRequest>,
    pending: Arc<Mutex<PendingMap>>,
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
                let permission = policy.answer(grant, decision).await;
                let _ = respond.send(permission);
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
    use std::path::{Path, PathBuf};
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

    fn state_of(policy: Policy, grants: &[(&str, Option<&str>)]) -> PolicyState {
        let dir = std::env::temp_dir().join(format!("alan-policy-state-{}", std::process::id()));
        PolicyState {
            store: Arc::new(PermissionStore::new(dir.join("permissions.json"))),
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

    #[tokio::test]
    async fn respond_unblocks_authorize() {
        let policy = session_policy("respond");
        let manager = AlanPermissionManager::init(policy);
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id, Answer::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }

    #[tokio::test]
    async fn strict_allows_after_approval() {
        let (policy, _path) = temp_policy("strict");
        let manager1 = AlanPermissionManager::init(policy.clone());
        let handler1 = manager1.handler();

        // First call: strict mode asks, user approves.
        let handle = tokio::spawn(async move { manager1.authorize(&shell("bun dev")).await });
        let request = handler1.subscribe().next().await.expect("request");
        handler1.respond(request.id, Answer::Allowed);
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
        let policy = session_policy("denied");
        let manager = AlanPermissionManager::init(policy);
        // Never subscribe: the actor finds no listeners and drops the
        // responder, so authorize denies instead of hanging.
        assert_eq!(manager.authorize(&call("bash")).await, Permission::Denied);
    }

    #[tokio::test]
    async fn stale_respond_is_ignored() {
        let policy = session_policy("stale");
        let manager = AlanPermissionManager::init(policy);
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id + 999, Answer::Denied);
        handler.respond(request.id, Answer::Allowed);
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }

    #[tokio::test]
    async fn free_policy_allows_without_prompting() {
        let (policy, _path) = temp_policy("free");
        policy.set_policy(Policy::Free);
        let manager = AlanPermissionManager::init(policy);
        assert_eq!(
            manager.authorize(&shell("bun dev --host")).await,
            Permission::Allowed
        );
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

    fn temp_policy(name: &str) -> (ToolPolicy, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("alan-policy-test-{}-{name}", std::process::id()));
        let path = dir.join("permissions.json");
        (ToolPolicy::new(Arc::new(PermissionStore::new(&path))), path)
    }

    /// A policy whose store is never written to; for session-only tests.
    fn session_policy(name: &str) -> ToolPolicy {
        let dir =
            std::env::temp_dir().join(format!("alan-policy-session-{}-{name}", std::process::id()));
        ToolPolicy::new(Arc::new(PermissionStore::new(dir.join("permissions.json"))))
    }

    fn temp_dir_of(path: &Path) -> PathBuf {
        path.parent().unwrap().parent().unwrap().to_owned()
    }

    #[tokio::test]
    async fn allowed_always_persists_and_survives_reload() {
        let (policy, path) = temp_policy("always");
        let grant = Grant::of(&shell("bun dev --host"));
        let _ = policy.answer(grant.clone(), Answer::AllowedAlways).await;

        let reloaded = ToolPolicy::new(Arc::new(PermissionStore::new(&path)));
        assert_eq!(
            reloaded.check(&shell("bun dev --host")).await,
            Decision::Allow
        );
        dbg!(&reloaded.0.lock().expect("lock").policy);
        assert_eq!(reloaded.check(&shell("bun test")).await, Decision::Ask);
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn allowed_session_does_not_persist() {
        let (policy, path) = temp_policy("session");
        let grant = Grant::of(&shell("bun dev"));
        let _ = policy.answer(grant.clone(), Answer::AllowedSession).await;
        assert!(!path.exists(), "session grants must not touch the store");
        // Still allowed in memory for this policy instance.
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Allow);
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn hashset_is_consulted_before_store() {
        let (policy, path) = temp_policy("lazy");
        // Seed only the in-memory set; the file must never be read/created.
        policy
            .0
            .lock()
            .expect("policy lock")
            .grants
            .insert(Grant::of(&shell("bun dev")));
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Allow);
        assert!(!path.exists(), "HashSet hit must not consult the store");
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn store_hit_backfills_hashset() {
        let (policy, path) = temp_policy("backfill");
        policy
            .answer(Grant::of(&shell("cargo test")), Answer::AllowedAlways)
            .await;

        // Same grants but a fresh policy whose in-memory set was backfilled
        // by the earlier store hit. Slip mode so the family grant ("cargo",
        // any args) is consulted.
        let (detached, _detached_path) = temp_policy("detached");
        detached
            .0
            .lock()
            .expect("policy lock")
            .grants
            .extend(policy.0.lock().expect("policy lock").grants.clone());
        detached.set_policy(Policy::Slip);
        assert_eq!(detached.check(&shell("cargo test")).await, Decision::Allow);
        assert_eq!(detached.check(&cargo_other()).await, Decision::Allow);
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    fn cargo_other() -> ToolCall {
        ToolCall {
            id: "call-2".into(),
            name: "bash".into(),
            arguments: "cargo clippy".into(),
            signature: None,
        }
    }

    #[tokio::test]
    async fn deny_and_stop_never_persist() {
        for answer in [Answer::Denied, Answer::DeniedSession, Answer::Stop] {
            let (policy, path) = temp_policy("deny-stop");
            let grant = Grant::of(&shell("bun dev"));
            let permission = policy.answer(grant, answer.clone()).await;
            assert_eq!(permission, Permission::Denied);
            assert!(!path.exists(), "{answer:?} must not touch the store");
            let _ = std::fs::remove_dir_all(temp_dir_of(&path));
        }
    }

    #[tokio::test]
    async fn stop_unblocks_authorize_with_denied() {
        let policy = session_policy("stop");
        let manager = AlanPermissionManager::init(policy);
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&shell("bun dev")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id, Answer::Stop);
        assert_eq!(handle.await.expect("task"), Permission::Denied);
    }
}
