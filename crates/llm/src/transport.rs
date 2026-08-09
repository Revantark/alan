use crate::{Credential, LlmError};

/// Reusable HTTP client shared by API implementations.
pub struct HttpClient {
    client: reqwest::Client,
}

impl HttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub async fn post(
        &self,
        url: &str,
        body: &str,
        credential: Option<&Credential>,
    ) -> Result<reqwest::Response, LlmError> {
        let request = self
            .client
            .post(url)
            .header("Content-Type", "application/json");
        let request = match credential {
            Some(Credential::ApiKey(value) | Credential::BearerToken(value)) => {
                request.bearer_auth(value)
            }
            Some(Credential::None) | None => request,
        };

        request
            .body(body.to_owned())
            .send()
            .await
            .map_err(LlmError::Transport)
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}
