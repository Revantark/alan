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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

/// Split a shell command on `&&`, `||`, `;` and `|`. Quoted or escaped
/// operators are data, not separators.
fn split_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;

    let mut quote: Option<u8> = None;

    while index < bytes.len() {
        let byte = bytes[index];

        if let Some(q) = quote {
            if byte == b'\\' && q == b'"' {
                index += 2;
                continue;
            }
            if byte == q {
                quote = None;
            }
            index += 1;
            continue;
        }

        match byte {
            b'\\' => {
                index += 2;
                continue;
            }
            b'\'' | b'"' => {
                quote = Some(byte);
                index += 1;
                continue;
            }
            // `>&2`/`2>&1` are one token to the shell; splitting there would
            // grant a `1` command the shell never runs.
            b'>' | b'<' => {
                index += 1;
                if index < bytes.len() && matches!(bytes[index], b'&' | b'>' | b'<') {
                    index += 1;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                }
                continue;
            }
            b'&' | b';' | b'|' => {
                segments.push(&command[start..index]);
                // Two-byte operators, so the second byte is not a separator.
                if index + 1 < bytes.len() && bytes[index + 1] == byte {
                    index += 2;
                } else {
                    index += 1;
                }
                start = index;
                continue;
            }
            _ => index += 1,
        }
    }
    segments.push(&command[start..]);

    segments
        .into_iter()
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

/// Grant key for one segment: the first word is the family, the rest is the
/// exact-args suffix. A single-word segment is a family grant.
fn grant_for_segment(segment: &str) -> Grant {
    let words = shlex::split(segment)
        .map(|words| {
            words
                .into_iter()
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if words.is_empty() {
        let normalized = segment.split_whitespace().collect::<Vec<_>>().join(" ");
        return Grant::new(normalized, None);
    }

    let (command, args) = words.split_first().expect("non-empty words");
    if args.is_empty() {
        return Grant::new((*command).to_owned(), None);
    }

    Grant::new((*command).to_owned(), Some(args.join(" ")))
}

/// Grants implied by a tool call. Only `command`/`path` key a grant, so the
/// model's self-reported `kind` and file content cannot invalidate one.
fn grants_for_call(name: &str, arguments: &str) -> Vec<Grant> {
    let subject = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| match name {
            "bash" => value
                .get("command")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            "read" | "write" | "edit" => value
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            _ => Some(value.to_string()),
        });

    let Some(subject) = subject else {
        return vec![Grant::new(name.to_owned(), None)];
    };

    if name != "bash" {
        return vec![Grant::new(name.to_owned(), Some(subject))];
    }

    // A string of only separators decomposes to nothing; gate it as a blob.
    let segments = split_segments(&subject);
    if segments.is_empty() {
        return vec![Grant::new(name.to_owned(), None)];
    }

    segments.into_iter().map(grant_for_segment).collect()
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
    pub fn new(command: String, args: Option<String>) -> Self {
        Self { command, args }
    }

    /// Every grant this call implies.
    pub fn all_of(call: &ToolCall) -> Vec<Grant> {
        grants_for_call(&call.name, &call.arguments)
    }

    /// The family grant this one would promote to under Slip mode.
    fn family(&self) -> Self {
        Self {
            command: self.command.clone(),
            args: None,
        }
    }

    fn is_family_of(&self, tool: &str) -> bool {
        self.args.is_none() && self.command == tool
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

/// The user's answer to a tool-authorization prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Allow this one call. Nothing is recorded.
    Allowed,
    /// Allow this call and its family for the rest of the session.
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
/// never used inside the pure `decide`/`allows`/`grant_session` methods.
pub struct PolicyState {
    pub policy: Policy,
    pub grants: HashSet<Grant>,
    pub denied: HashSet<Grant>,
    /// Whether the store has already been drained into `grants`.
    pub store_loaded: bool,
    pub store: Arc<PermissionStore>,
}

impl PolicyState {
    /// Whether one grant is covered by the current grants.
    fn allows(&self, grant: &Grant) -> bool {
        match self.policy {
            Policy::Free => true,
            Policy::Strict => self.grants.contains(grant),
            Policy::Slip => self.grants.contains(grant) || self.grants.contains(&grant.family()),
        }
    }

    /// Whether this grant, or the family it belongs to, was denied for the
    /// session.
    fn is_denied(&self, grant: &Grant) -> bool {
        self.denied.contains(grant) || self.denied.contains(&grant.family())
    }

    /// One edit/write approval covers both, so scan instead of probing.
    fn unlocks_edit_tools(&self) -> bool {
        self.grants
            .iter()
            .any(|grant| grant.is_family_of("edit") || grant.is_family_of("write"))
    }

    /// Grants are passed in rather than derived here so the caller resolves
    /// them once per call. A chain is allowed only if *every* command in it
    /// is covered: a partially approved chain would run its unapproved half
    /// unseen.
    fn decide(&self, call: &ToolCall, grants: &[Grant]) -> Decision {
        if self.policy == Policy::Free {
            return Decision::Allow;
        }

        // One edit-tool approval unlocks all edit tools, so this deliberately
        // precedes the per-grant rules.
        if self.policy == Policy::Strict
            && matches!(call.name.as_str(), "edit" | "write")
            && self.unlocks_edit_tools()
        {
            return Decision::Allow;
        }

        if grants.iter().any(|grant| self.is_denied(grant)) {
            return Decision::Deny;
        }

        if grants.iter().all(|grant| self.allows(grant)) {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }

    /// Exact grants plus their families, so siblings stop prompting. A later
    /// allow clears an earlier deny: the most recent decision wins.
    fn grant_session(&mut self, grants: &[Grant]) {
        for grant in grants {
            self.grants.insert(grant.family());
            self.grants.insert(grant.clone());
            self.denied.remove(grant);
            self.denied.remove(&grant.family());
        }
    }

    /// Session-only; the store is never touched.
    fn deny_session(&mut self, grants: &[Grant]) {
        for grant in grants {
            self.denied.insert(grant.family());
            self.denied.insert(grant.clone());
        }
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
            denied: HashSet::new(),
            store_loaded: false,
            store,
        })))
    }

    /// In-memory grants first; the store only on a miss, and only once. The
    /// lock is never held across the store read.
    pub async fn check(&self, call: &ToolCall) -> Decision {
        let grants = Grant::all_of(call);

        let (decision, store, store_loaded) = {
            let state = self.0.lock().expect("policy lock");
            (
                state.decide(call, &grants),
                state.store.clone(),
                state.store_loaded,
            )
        };
        if decision == Decision::Deny || store_loaded {
            return decision;
        }

        // A miss leaves nothing in memory to remember the empty set by, and
        // `answer` records into the in-memory set, so reading again per call
        // would only re-pay the read, lock and parse.
        match store.load_all().await {
            Ok(persisted) => {
                let mut state = self.0.lock().expect("policy lock");
                state.store_loaded = true;
                state.grants.extend(persisted);
                state.decide(call, &grants)
            }
            Err(error) => {
                warn!(%error, "failed to read persisted permissions");
                Decision::Ask
            }
        }
    }

    /// Answer a pending request over all of the grants the call implies, and
    /// translate it to the agent's two-way [`Permission`].
    pub async fn answer(&self, grants: &[Grant], answer: Answer) -> Permission {
        if answer == Answer::DeniedSession {
            self.0.lock().expect("policy lock").deny_session(grants);
            return Permission::Denied;
        }

        if !answer.allows() {
            return answer.into();
        }

        // `Allowed` is a one-shot pass: no session grant, no persistence.
        if answer == Answer::Allowed {
            return Permission::Allowed;
        }

        let store = {
            let mut state = self.0.lock().expect("policy lock");
            state.grant_session(grants);
            state.store.clone()
        };

        if answer != Answer::AllowedAlways {
            return answer.into();
        }

        // The family entry is what keeps an approved command working across
        // runs, so persist it alongside the exact grant. Collecting both
        // first keeps a compound command to one read-modify-write.
        let to_persist: Vec<Grant> = grants
            .iter()
            .flat_map(|grant| {
                let family = grant.args.is_some().then(|| grant.family());
                [Some(grant.clone()), family]
            })
            .flatten()
            .collect();

        if let Err(error) = store.insert_all(&to_persist).await {
            warn!(%error, "failed to persist permission grants");
        }

        answer.into()
    }

    pub fn set_policy(&self, policy: Policy) {
        self.0.lock().expect("policy lock").policy = policy;
    }

    /// The currently active policy mode.
    pub fn policy(&self) -> Policy {
        self.0.lock().expect("policy lock").policy
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

        match self.policy.check(call).await {
            Decision::Allow => return Permission::Allowed,
            // Session-denied: refuse instead of prompting again.
            Decision::Deny => return Permission::Denied,
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
        futures_util::stream::unfold(self.requests.subscribe(), move |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(request) => return Some((request, rx)),
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(skipped, "permission request stream lagged");
                    }
                    Err(RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }

    pub async fn respond(&self, id: u64, decision: Answer) {
        if let Err(error) = self.commands.send(Command::Respond { id, decision }).await {
            warn!(%error, id, "failed to deliver permission answer");
        }
    }
}

type PendingMap = HashMap<u64, (oneshot::Sender<Permission>, Vec<Grant>)>;

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
                // Resolved here, while the raw arguments are in hand, so the
                // answer path never re-parses the call.
                let grants = Grant::all_of(&ToolCall {
                    id: id.to_string(),
                    name: request.name.clone(),
                    arguments: request.arguments.clone(),
                    signature: None,
                });

                if request_tx.send(request).is_err() {
                    drop(respond);
                } else {
                    pending
                        .lock()
                        .expect("pending lock")
                        .insert(id, (respond, grants));
                }
            }
            Command::Respond { id, decision } => {
                let Some((respond, grants)) = pending.lock().expect("pending lock").remove(&id)
                else {
                    continue;
                };
                let permission = policy.answer(&grants, decision).await;
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

    /// Arguments arrive as a JSON object, including the model's own `kind`
    /// label, which must not leak into the grant key.
    fn shell(command: &str) -> ToolCall {
        bash_with_kind(command, "write")
    }

    fn bash_with_kind(command: &str, kind: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "kind": kind, "command": command }).to_string(),
            signature: None,
        }
    }

    /// Carries file content, which must not become part of the grant key.
    fn write_call(path: &str, content: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            name: "write".into(),
            arguments: serde_json::json!({ "path": path, "content": content }).to_string(),
            signature: None,
        }
    }

    fn state_of(policy: Policy, grants: &[(&str, Option<&str>)]) -> PolicyState {
        state_denied(policy, grants, &[])
    }

    /// A `PolicyState` with session denies already recorded.
    fn state_denied(
        policy: Policy,
        grants: &[(&str, Option<&str>)],
        denied: &[(&str, Option<&str>)],
    ) -> PolicyState {
        let dir = std::env::temp_dir().join(format!("alan-policy-state-{}", std::process::id()));
        let to_grants = |list: &[(&str, Option<&str>)]| {
            list.iter()
                .map(|(command, args)| Grant {
                    command: (*command).into(),
                    args: args.map(|args| args.into()),
                })
                .collect()
        };
        PolicyState {
            store: Arc::new(PermissionStore::new(dir.join("permissions.json"))),
            store_loaded: true,
            policy,
            grants: to_grants(grants),
            denied: to_grants(denied),
        }
    }

    #[tokio::test]
    async fn respond_unblocks_authorize() {
        let policy = session_policy("respond");
        let manager = AlanPermissionManager::init(policy);
        let handler = manager.handler();
        let handle = tokio::spawn(async move { manager.authorize(&call("bash")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id, Answer::Allowed).await;
        assert_eq!(handle.await.expect("task"), Permission::Allowed);
    }

    #[tokio::test]
    async fn a_dropped_subscriber_denies_rather_than_hanging() {
        // No subscriber means the actor finds no listeners and drops the
        // responder: deny rather than wait out the timeout.
        let policy = session_policy("no-subscriber");
        let manager = AlanPermissionManager::init(policy);
        assert_eq!(
            manager.authorize(&shell("bun dev")).await,
            Permission::Denied
        );
    }

    #[tokio::test]
    async fn strict_allows_after_approval() {
        let (policy, _path) = temp_policy("strict");
        let manager1 = AlanPermissionManager::init(policy.clone());
        let handler1 = manager1.handler();

        // Strict mode asks; the user approves for the session.
        let handle = tokio::spawn(async move { manager1.authorize(&shell("bun dev")).await });
        let request = handler1.subscribe().next().await.expect("request");
        handler1.respond(request.id, Answer::AllowedSession).await;
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
    async fn allowed_remembers_nothing_and_reprompts() {
        // Regression: `Allowed` once inserted the family grant too, unlocking
        // every sibling command for the session.
        let (policy, path) = temp_policy("allowed-once");
        let permission = policy
            .answer(&Grant::all_of(&shell("bun dev")), Answer::Allowed)
            .await;
        assert_eq!(permission, Permission::Allowed);
        assert!(!path.exists(), "allow-once must not touch the store");

        // Nothing remembered: the same call asks again, and so does a sibling.
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Ask);
        assert_eq!(policy.check(&shell("bun test")).await, Decision::Ask);
    }

    #[tokio::test]
    async fn allowed_session_unlocks_the_family_but_never_persists() {
        let (policy, path) = temp_policy("allowed-session");
        let permission = policy
            .answer(&Grant::all_of(&shell("bun dev")), Answer::AllowedSession)
            .await;
        assert_eq!(permission, Permission::Allowed);
        assert!(!path.exists(), "session grants must not touch the store");

        // The exact call and its siblings are both unlocked for this session.
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Allow);
        policy.set_policy(Policy::Slip);
        assert_eq!(
            policy.check(&bash_with_kind("bun test", "read")).await,
            Decision::Allow
        );
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn allowed_always_persists_both_exact_and_family() {
        let (policy, path) = temp_policy("allowed-always");
        policy
            .answer(&Grant::all_of(&shell("bun dev")), Answer::AllowedAlways)
            .await;

        let persisted = PermissionStore::new(&path).load_all().await.unwrap();
        assert!(
            persisted.contains(&Grant::new("bun".into(), None)),
            "the family grant must survive a restart, got {persisted:?}"
        );
        assert!(persisted.contains(&Grant::all_of(&shell("bun dev")).remove(0)));
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
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
        handler.respond(request.id + 999, Answer::Denied).await;
        handler.respond(request.id, Answer::Allowed).await;
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

    /// `decide` for a call, resolving the grants the way `check` does.
    fn decides(state: &PolicyState, call: &ToolCall) -> Decision {
        state.decide(call, &Grant::all_of(call))
    }

    #[test]
    fn strict_allows_only_exact_grants() {
        let state = state_of(Policy::Strict, &[("bun", Some("dev")), ("cargo", None)]);
        assert_eq!(decides(&state, &shell("bun dev")), Decision::Allow);
        assert_eq!(decides(&state, &shell("bun dev --host")), Decision::Ask);
        // A family grant is not consulted in strict mode.
        assert_eq!(decides(&state, &shell("cargo test")), Decision::Ask);
        assert_eq!(decides(&state, &shell("node -v")), Decision::Ask);
    }

    #[test]
    fn strict_unlocks_all_edit_tools_after_one_approval() {
        // One edit approval unlocks any edit/write call, args ignored.
        let state = state_of(
            Policy::Strict,
            &[("edit", Some("{\"path\":\"a.rs\"}")), ("edit", None)],
        );
        assert_eq!(decides(&state, &call("edit")), Decision::Allow);
        assert_eq!(decides(&state, &call("write")), Decision::Allow);
        // Non-edit tools are unaffected.
        assert_eq!(decides(&state, &shell("bun dev")), Decision::Ask);

        let state = state_of(Policy::Strict, &[("write", None)]);
        assert_eq!(decides(&state, &call("edit")), Decision::Allow);
    }

    #[test]
    fn strict_still_asks_for_edit_tools_without_grants() {
        let state = state_of(Policy::Strict, &[("bash", Some("ls"))]);
        assert_eq!(decides(&state, &call("edit")), Decision::Ask);
        assert_eq!(decides(&state, &call("write")), Decision::Ask);
    }

    #[test]
    fn grant_of_normalizes_whitespace_and_splits_family() {
        let grant = Grant::all_of(&shell("  bun \t dev   --host ")).remove(0);
        assert_eq!(
            grant,
            Grant {
                command: "bun".into(),
                args: Some("dev --host".into())
            }
        );
        // Single-word arguments: the whole string is the family.
        assert_eq!(
            Grant::all_of(&shell("bun")).remove(0),
            Grant {
                command: "bun".into(),
                args: None
            }
        );
    }

    #[test]
    fn grant_key_ignores_the_model_supplied_kind() {
        // Regression: the key was built from raw JSON, so the model's own
        // `kind` label became part of the command family.
        assert_eq!(
            Grant::all_of(&shell("bun dev")).remove(0),
            Grant::all_of(&bash_with_kind("bun dev", "read")).remove(0)
        );
        assert_eq!(
            Grant::all_of(&bash_with_kind("bun dev", "network")).remove(0),
            Grant {
                command: "bun".into(),
                args: Some("dev".into())
            }
        );
    }

    #[test]
    fn file_tool_grants_are_keyed_on_path_not_content() {
        // Regression: these keys once embedded the whole argument object,
        // so an exact grant could never match again after an edit.
        assert_eq!(
            Grant::all_of(&write_call("a.rs", "one")).remove(0),
            Grant::all_of(&write_call("a.rs", "two")).remove(0)
        );
        assert_ne!(
            Grant::all_of(&write_call("a.rs", "one")).remove(0),
            Grant::all_of(&write_call("b.rs", "one")).remove(0)
        );
        assert_eq!(
            Grant::all_of(&write_call("a.rs", "body")).remove(0),
            Grant {
                command: "write".into(),
                args: Some("a.rs".into())
            }
        );
    }

    #[test]
    fn malformed_arguments_key_on_the_tool_name_alone() {
        // Unparseable arguments must still gate deterministically.
        let broken = ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: "not json".into(),
            signature: None,
        };
        assert_eq!(
            Grant::all_of(&broken),
            vec![Grant {
                command: "bash".into(),
                args: None
            }]
        );

        // Valid JSON but the expected field is missing.
        let missing = ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"kind":"write"}"#.into(),
            signature: None,
        };
        assert_eq!(Grant::all_of(&missing), Grant::all_of(&broken));
    }

    #[test]
    fn slip_unlocks_siblings_regardless_of_kind() {
        // Regression: a `kind` change used to split the family, so Slip never
        // unlocked a sibling command.
        let mut state = state_of(Policy::Slip, &[]);
        state.grant_session(&Grant::all_of(&shell("bun dev")));
        assert_eq!(
            decides(&state, &bash_with_kind("bun test", "read")),
            Decision::Allow
        );
        assert_eq!(
            decides(&state, &bash_with_kind("bun run build", "network")),
            Decision::Allow
        );
        // A different family is still gated.
        assert_eq!(
            decides(&state, &bash_with_kind("cargo test", "read")),
            Decision::Ask
        );
    }

    #[test]
    fn strict_ignores_kind_changes_for_the_same_command() {
        // Regression: relabelling used to force a re-prompt for a
        // byte-identical command.
        let state = state_of(Policy::Strict, &[("bun", Some("dev"))]);
        assert_eq!(
            decides(&state, &bash_with_kind("bun dev", "write")),
            Decision::Allow
        );
        assert_eq!(
            decides(&state, &bash_with_kind("bun dev", "network")),
            Decision::Allow
        );
        // Different args are still gated.
        assert_eq!(
            decides(&state, &bash_with_kind("bun dev --host", "write")),
            Decision::Ask
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

    /// The test's own directory, so cleanup never reaches a sibling's.
    fn temp_dir_of(path: &Path) -> PathBuf {
        path.parent().unwrap().to_owned()
    }

    #[tokio::test]
    async fn allowed_always_persists_and_survives_reload() {
        let (policy, path) = temp_policy("always");
        let grants = Grant::all_of(&shell("bun dev --host"));
        let _ = policy.answer(&grants, Answer::AllowedAlways).await;

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
        let grants = Grant::all_of(&shell("bun dev"));
        let _ = policy.answer(&grants, Answer::AllowedSession).await;
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
            .insert(Grant::all_of(&shell("bun dev")).remove(0));
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Allow);
        assert!(!path.exists(), "HashSet hit must not consult the store");

        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn a_store_miss_is_not_re_read_for_every_call() {
        // Regression: every gated call re-read and re-parsed
        // `permissions.json` for a set already known to be empty.
        let (policy, path) = temp_policy("miss");
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Ask);
        assert!(policy.0.lock().expect("policy lock").store_loaded);
        assert_eq!(policy.check(&shell("bun test")).await, Decision::Ask);
        assert_eq!(policy.check(&shell("bun run")).await, Decision::Ask);

        // Skipping the store stays safe: `answer` records in memory.
        let _ = policy
            .answer(&Grant::all_of(&shell("bun dev")), Answer::AllowedSession)
            .await;
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Allow);

        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn a_corrupt_store_keeps_asking() {
        let (policy, path) = temp_policy("corrupt");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        std::fs::write(&path, r#"{"allow": "not-a-list"}"#).unwrap();

        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Ask);
        // Not marked loaded, so a repaired file is still picked up.
        assert!(!policy.0.lock().expect("policy lock").store_loaded);

        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn denied_session_refuses_without_re_prompting() {
        // Regression: this used to behave like `Denied`, so the second
        // identical call prompted again.
        let (policy, path) = temp_policy("deny-session");
        let grants = Grant::all_of(&shell("rm -rf /tmp/x"));
        assert_eq!(
            policy.answer(&grants, Answer::DeniedSession).await,
            Permission::Denied
        );

        assert_eq!(policy.check(&shell("rm -rf /tmp/x")).await, Decision::Deny);
        assert_eq!(policy.check(&shell("rm -rf /tmp/y")).await, Decision::Deny);
        assert_eq!(policy.check(&shell("bun dev")).await, Decision::Ask);
        // Never persisted.
        assert!(!path.exists(), "session denies must not touch the store");
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn denied_session_survives_a_reload_as_nothing() {
        let (policy, path) = temp_policy("deny-reload");
        let _ = policy
            .answer(
                &Grant::all_of(&shell("rm -rf /tmp/x")),
                Answer::DeniedSession,
            )
            .await;
        let reloaded = ToolPolicy::new(Arc::new(PermissionStore::new(&path)));
        assert_eq!(reloaded.check(&shell("rm -rf /tmp/x")).await, Decision::Ask);
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn plain_denied_still_re_prompts() {
        // The one-shot deny records nothing, so it must not pick up
        // session-deny behavior by accident.
        let (policy, path) = temp_policy("deny-once");
        let grants = Grant::all_of(&shell("rm -rf /tmp/x"));
        assert_eq!(
            policy.answer(&grants, Answer::Denied).await,
            Permission::Denied
        );
        assert_eq!(policy.check(&shell("rm -rf /tmp/x")).await, Decision::Ask);
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[test]
    fn a_session_deny_outranks_an_existing_allow() {
        // The family allow for `bun` must not override the `bun dev` deny.
        let state = state_denied(Policy::Slip, &[("bun", None)], &[("bun", Some("dev"))]);
        assert_eq!(decides(&state, &shell("bun dev")), Decision::Deny);
        // An un-denied sibling is still allowed.
        assert_eq!(decides(&state, &shell("bun test")), Decision::Allow);
    }

    #[test]
    fn a_later_allow_clears_a_session_deny() {
        // The most recent decision wins over an earlier deny.
        let mut state = state_denied(Policy::Slip, &[], &[("bun", Some("dev"))]);
        state.grant_session(&Grant::all_of(&shell("bun dev")));
        assert_eq!(decides(&state, &shell("bun dev")), Decision::Allow);
    }

    #[test]
    fn a_deny_in_a_chain_refuses_the_whole_call() {
        // Half a chain must not run: the other half would execute unseen.
        let state = state_denied(
            Policy::Strict,
            &[("cargo", Some("test"))],
            &[("rg", Some("x"))],
        );
        assert_eq!(
            decides(&state, &shell("cargo test && rg x")),
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn authorize_denies_a_session_denied_call_without_a_prompt() {
        let policy = session_policy("deny-authorize");
        let manager = Arc::new(AlanPermissionManager::init(policy));
        let handler = manager.handler();
        let spawned = Arc::clone(&manager);
        let handle = tokio::spawn(async move { spawned.authorize(&shell("rm -rf /tmp/x")).await });
        let request = handler.subscribe().next().await.expect("request");
        handler.respond(request.id, Answer::DeniedSession).await;
        assert_eq!(handle.await.expect("task"), Permission::Denied);

        // The same call again: refused without a request reaching the UI.
        let mut requests = handler.subscribe();
        assert_eq!(
            manager.authorize(&shell("rm -rf /tmp/x")).await,
            Permission::Denied
        );
        assert!(
            futures_util::poll!(requests.next()).is_pending(),
            "a session-denied call must not prompt again"
        );
    }

    #[tokio::test]
    async fn store_hit_backfills_hashset() {
        let (policy, path) = temp_policy("backfill");
        policy
            .answer(&Grant::all_of(&shell("cargo test")), Answer::AllowedAlways)
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
            arguments: serde_json::json!({ "kind": "read", "command": "cargo clippy" }).to_string(),
            signature: None,
        }
    }

    #[tokio::test]
    async fn deny_and_stop_never_persist() {
        for answer in [Answer::Denied, Answer::DeniedSession, Answer::Stop] {
            let (policy, path) = temp_policy("deny-stop");
            let grants = Grant::all_of(&shell("bun dev"));
            let permission = policy.answer(&grants, answer.clone()).await;
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
        handler.respond(request.id, Answer::Stop).await;
        assert_eq!(handle.await.expect("task"), Permission::Denied);
    }

    fn grant_of(command: &str, args: Option<&str>) -> Grant {
        Grant::new(command.into(), args.map(Into::into))
    }

    #[test]
    fn chained_commands_become_one_grant_each() {
        assert_eq!(
            Grant::all_of(&shell(r#"rg "something" && cargo test && cargo run"#)),
            vec![
                grant_of("rg", Some("something")),
                grant_of("cargo", Some("test")),
                grant_of("cargo", Some("run")),
            ]
        );
    }

    #[test]
    fn every_chaining_operator_splits() {
        for command in [
            "cargo test && cargo run",
            "cargo test || cargo run",
            "cargo test; cargo run",
            "cargo test | cargo run",
        ] {
            assert_eq!(
                Grant::all_of(&shell(command)),
                vec![
                    grant_of("cargo", Some("test")),
                    grant_of("cargo", Some("run"))
                ],
                "must split on the operator in {command:?}"
            );
        }
    }

    #[test]
    fn operator_inside_quotes_is_not_a_separator() {
        // The guard against a naive `split("&&")`.
        assert_eq!(
            Grant::all_of(&shell(r#"rg "a && b""#)),
            vec![grant_of("rg", Some("a && b"))]
        );
        assert_eq!(
            Grant::all_of(&shell("rg 'x; y' && cargo test")),
            vec![
                grant_of("rg", Some("x; y")),
                grant_of("cargo", Some("test"))
            ]
        );
        // The escaped quote never closes the string, so the shell sees one
        // command: it stays one opaque grant rather than being split into
        // grants the shell would never run.
        assert_eq!(
            Grant::all_of(&shell(r#"rg "a\" && cargo test"#)),
            vec![grant_of("rg \"a\\\" && cargo test", None)]
        );
    }

    #[test]
    fn redirection_operators_do_not_split() {
        // Regression: splitting `2>&1` produced a phantom `1` command the
        // shell never runs.
        assert_eq!(
            Grant::all_of(&shell("ls 2>&1")),
            vec![grant_of("ls", Some("2>&1"))]
        );
        assert_eq!(
            Grant::all_of(&shell("cargo test > out.log 2>&1")),
            vec![grant_of("cargo", Some("test > out.log 2>&1"))]
        );

        assert_eq!(
            Grant::all_of(&shell("ls 2>&1 && cargo test")),
            vec![
                grant_of("ls", Some("2>&1")),
                grant_of("cargo", Some("test"))
            ]
        );
    }

    #[test]
    fn a_digit_argument_does_not_swallow_the_rest_of_the_command() {
        // Regression: a branch fired on *any* digit and swallowed the
        // operator behind it, so `echo 2;rm -rf ~` stayed one segment gated
        // only on the `echo` family an earlier `echo hi` had unlocked.
        assert_eq!(
            Grant::all_of(&shell("echo 2;ls")),
            vec![grant_of("echo", Some("2")), grant_of("ls", None)]
        );
        assert_eq!(
            Grant::all_of(&shell("cat a 3;rm -rf /")),
            vec![grant_of("cat", Some("a 3")), grant_of("rm", Some("-rf /"))]
        );
        assert_eq!(
            Grant::all_of(&shell("echo 2&&ls")),
            vec![grant_of("echo", Some("2")), grant_of("ls", None)]
        );
        assert_eq!(
            Grant::all_of(&shell("echo 2|ls")),
            vec![grant_of("echo", Some("2")), grant_of("ls", None)]
        );

        assert_eq!(
            Grant::all_of(&shell("ls 2>/tmp/x;cat /etc/hosts")),
            vec![
                grant_of("ls", Some("2>/tmp/x")),
                grant_of("cat", Some("/etc/hosts"))
            ]
        );
    }

    #[test]
    fn a_leading_redirection_keeps_the_segment_whole() {
        // `2>/tmp/x` names a descriptor, not a command: it stays one gate and
        // keeps its text rather than losing the leading `2` to the family.
        assert_eq!(
            Grant::all_of(&shell("2>/tmp/x;id")),
            vec![grant_of("2>/tmp/x", None), grant_of("id", None)]
        );
    }

    #[test]
    fn empty_segments_are_dropped() {
        assert_eq!(
            Grant::all_of(&shell("cargo test &&")),
            vec![grant_of("cargo", Some("test"))]
        );
        assert_eq!(
            Grant::all_of(&shell(";; cargo test ;;")),
            vec![grant_of("cargo", Some("test"))]
        );

        assert_eq!(Grant::all_of(&shell("&&")), vec![grant_of("bash", None)]);
    }

    #[test]
    fn hallucinated_args_field_is_never_granted() {
        // `args` is not in the tool schema and the executor never reads it.
        let call = ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({
                "args": "/Users/rev/Projects/alan && cargo run",
                "command": "cd",
            })
            .to_string(),
            signature: None,
        };
        assert_eq!(Grant::all_of(&call), vec![grant_of("cd", None)]);
    }

    #[test]
    fn strict_requires_every_command_in_a_chain() {
        let mut state = state_of(Policy::Strict, &[]);
        state.grant_session(&[grant_of("cargo", Some("test"))]);

        assert_eq!(
            decides(&state, &shell("cargo test && cargo run")),
            Decision::Ask
        );

        assert_eq!(decides(&state, &shell("cargo test")), Decision::Allow);
    }

    #[tokio::test]
    async fn allowed_always_persists_every_command_in_a_chain() {
        let (policy, path) = temp_policy("chain");
        policy
            .answer(
                &Grant::all_of(&shell("rg something && cargo test && cargo run")),
                Answer::AllowedAlways,
            )
            .await;

        let persisted = PermissionStore::new(&path).load_all().await.unwrap();
        for expected in [
            grant_of("rg", Some("something")),
            grant_of("cargo", Some("test")),
            grant_of("cargo", Some("run")),
        ] {
            assert!(persisted.contains(&expected), "missing {expected:?}");
        }

        let reloaded = ToolPolicy::new(Arc::new(PermissionStore::new(&path)));
        assert_eq!(
            reloaded
                .check(&shell("rg something && cargo test && cargo run"))
                .await,
            Decision::Allow
        );
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn repeated_command_in_a_chain_is_written_once() {
        let (policy, path) = temp_policy("chain-dedup");
        policy
            .answer(
                &Grant::all_of(&shell("cargo test && cargo test")),
                Answer::AllowedAlways,
            )
            .await;

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let allow = raw["allow"].as_array().unwrap();
        // The exact grant plus the `cargo` family; the duplicate segment
        // must not add a third.
        assert_eq!(allow.len(), 2, "duplicate segment was written twice");
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }

    #[tokio::test]
    async fn slip_family_covers_every_segment_of_a_chain() {
        let (policy, path) = temp_policy("chain-slip");
        policy
            .answer(&Grant::all_of(&shell("cargo test")), Answer::AllowedAlways)
            .await;
        policy.set_policy(Policy::Slip);

        // The `cargo` family grant covers both segments.
        assert_eq!(
            policy.check(&shell("cargo test && cargo run")).await,
            Decision::Allow
        );
        // A different family still gates the whole chain.
        assert_eq!(
            policy.check(&shell("cargo test && rg x")).await,
            Decision::Ask
        );
        let _ = std::fs::remove_dir_all(temp_dir_of(&path));
    }
}
