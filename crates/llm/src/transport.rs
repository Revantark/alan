use crate::{Credential, LlmError};
use std::time::Duration;

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
        let mut attempt = 0;
        loop {
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

            let response = request.body(body.to_owned()).send().await;

            match response {
                Ok(resp)
                    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 5 =>
                {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                    continue;
                }
                other => return other.map_err(LlmError::Transport),
            }
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}
