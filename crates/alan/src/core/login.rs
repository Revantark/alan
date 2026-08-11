use providers::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, CredentialStore, InteractionError,
    ProviderId, ProviderRegistry,
};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProvider {
    pub id: ProviderId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginState {
    Closed,
    Selecting {
        providers: Vec<LoginProvider>,
        selected: usize,
    },
    Prompting {
        provider: ProviderId,
        prompt: AuthPrompt,
    },
    Validating {
        provider: ProviderId,
        message: String,
    },
    Success {
        provider: ProviderId,
    },
    Error(String),
}

impl LoginState {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }
}

pub(crate) enum LoginMessage {
    Prompt {
        provider: ProviderId,
        prompt: AuthPrompt,
        response: oneshot::Sender<Result<String, InteractionError>>,
    },
    Event {
        provider: ProviderId,
        event: AuthEvent,
    },
    Finished {
        provider: ProviderId,
        result: Result<providers::Credential, providers::AuthError>,
    },
}

pub(crate) type LoginMessageSender = mpsc::UnboundedSender<LoginMessage>;
pub(crate) type LoginMessageReceiver = mpsc::UnboundedReceiver<LoginMessage>;

/// Owns authentication workflow state and its background task.
pub struct LoginController {
    providers: ProviderRegistry,
    credentials: Arc<dyn CredentialStore>,
    state: LoginState,
    receiver: Option<LoginMessageReceiver>,
    task: Option<JoinHandle<()>>,
    pending_prompt: Option<oneshot::Sender<Result<String, InteractionError>>>,
}

impl LoginController {
    pub fn new(providers: ProviderRegistry, credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            providers,
            credentials,
            state: LoginState::Closed,
            receiver: None,
            task: None,
            pending_prompt: None,
        }
    }

    pub fn state(&self) -> &LoginState {
        &self.state
    }

    pub fn open(&mut self) {
        let providers = self
            .providers
            .providers()
            .iter()
            .map(|provider| LoginProvider {
                id: provider.id().clone(),
                name: provider.id().0.clone(),
            })
            .collect();
        self.state = LoginState::Selecting {
            providers,
            selected: 0,
        };
    }

    pub fn submit(&mut self, text: String) {
        match &self.state {
            LoginState::Selecting { .. } => self.start_selected(),
            LoginState::Prompting { .. } => {
                if let Some(response) = self.pending_prompt.take() {
                    let _ = response.send(Ok(text));
                }
            }
            _ => {}
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let LoginState::Selecting {
            providers,
            selected,
        } = &mut self.state
        else {
            return;
        };
        if providers.is_empty() {
            return;
        }
        let max = providers.len().saturating_sub(1) as isize;
        *selected = (*selected as isize + delta).clamp(0, max) as usize;
    }

    pub fn cancel(&mut self) {
        if let Some(response) = self.pending_prompt.take() {
            let _ = response.send(Err(InteractionError::Cancelled));
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.receiver = None;
        self.state = LoginState::Closed;
    }

    pub fn poll(&mut self) -> super::Poll {
        let Some(mut receiver) = self.receiver.take() else {
            return super::Poll::Idle;
        };

        let mut changed = false;
        let mut finished = false;
        while let Ok(message) = receiver.try_recv() {
            changed = true;
            match message {
                LoginMessage::Prompt {
                    provider,
                    prompt,
                    response,
                } => {
                    self.pending_prompt = Some(response);
                    self.state = LoginState::Prompting { provider, prompt };
                }
                LoginMessage::Event { provider, event } => {
                    let message = match event {
                        AuthEvent::Info(message) | AuthEvent::Progress(message) => message,
                        AuthEvent::AuthUrl { url, instructions } => instructions
                            .map(|text| format!("{text}: {url}"))
                            .unwrap_or(url),
                    };
                    self.state = LoginState::Validating { provider, message };
                }
                LoginMessage::Finished { provider, result } => {
                    finished = true;
                    self.task = None;
                    self.pending_prompt = None;
                    match result {
                        Ok(_) => {
                            self.state = LoginState::Success { provider };
                        }
                        Err(error) => self.state = LoginState::Error(error.to_string()),
                    }
                }
            }
        }

        if !finished {
            self.receiver = Some(receiver);
        }
        if changed {
            super::Poll::Changed
        } else {
            super::Poll::Idle
        }
    }

    fn start_selected(&mut self) {
        let LoginState::Selecting {
            providers,
            selected,
        } = &self.state
        else {
            return;
        };
        let Some(provider_id) = providers.get(*selected).map(|provider| provider.id.clone()) else {
            self.state = LoginState::Error("No providers available".into());
            return;
        };
        self.start(provider_id);
    }

    fn start(&mut self, provider_id: ProviderId) {
        let Some(provider) = self.providers.get(&provider_id) else {
            self.state = LoginState::Error(format!("Unknown provider: {}", provider_id.0));
            return;
        };

        let (sender, receiver) = mpsc::unbounded_channel();
        let credentials = Arc::clone(&self.credentials);
        let provider_for_task = Arc::clone(&provider);
        let provider_for_state = provider_id.clone();
        let task_sender = sender.clone();
        self.receiver = Some(receiver);
        self.state = LoginState::Validating {
            provider: provider_for_state,
            message: "Starting login".into(),
        };
        self.task = Some(tokio::spawn(async move {
            let mut interaction = ChannelAuthInteraction {
                provider: provider_id.clone(),
                sender: task_sender,
            };
            let result = provider_for_task.auth().login(&mut interaction).await;
            let result = match result {
                Ok(credential) => credentials
                    .put(&provider_id, credential.clone())
                    .await
                    .map(|()| credential)
                    .map_err(AuthError::Storage),
                Err(error) => Err(error),
            };
            let _ = sender.send(LoginMessage::Finished {
                provider: provider_id,
                result,
            });
        }));
    }
}

struct ChannelAuthInteraction {
    provider: ProviderId,
    sender: LoginMessageSender,
}

#[async_trait::async_trait]
impl AuthInteraction for ChannelAuthInteraction {
    async fn prompt(&mut self, prompt: AuthPrompt) -> Result<String, InteractionError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(LoginMessage::Prompt {
                provider: self.provider.clone(),
                prompt,
                response: sender,
            })
            .map_err(|_| InteractionError::Cancelled)?;
        receiver.await.map_err(|_| InteractionError::Cancelled)?
    }

    fn notify(&mut self, event: AuthEvent) {
        let _ = self.sender.send(LoginMessage::Event {
            provider: self.provider.clone(),
            event,
        });
    }
}
