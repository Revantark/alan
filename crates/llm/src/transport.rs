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
        self.send(url, body, |request| match credential {
            Some(Credential::ApiKey(value)) => request.bearer_auth(value),
            Some(Credential::Header(name, value)) => request.header(name, value),
            Some(Credential::None) | None => request,
        })
        .await
    }

    async fn send<F>(
        &self,
        url: &str,
        body: &str,
        configure: F,
    ) -> Result<reqwest::Response, LlmError>
    where
        F: Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    {
        let mut attempt = 0;
        loop {
            let request = configure(
                self.client
                    .post(url)
                    .header("Content-Type", "application/json"),
            );

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
