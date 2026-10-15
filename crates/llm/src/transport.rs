use crate::{Credential, LlmError};
use std::time::Duration;

/// Upper bound for error bodies surfaced in [`LlmError::Http`], so HTML
/// challenge pages do not flood logs or the UI.
const MAX_ERROR_BODY: usize = 512;

fn truncate_bytes(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_string();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &value[..end])
}

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
        self.post_with_headers(url, body, credential, &[]).await
    }

    /// Like [`HttpClient::post`], attaching `headers` to the request as well.
    pub async fn post_with_headers(
        &self,
        url: &str,
        body: &str,
        credential: Option<&Credential>,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response, LlmError> {
        self.send(url, body, headers, |request| match credential {
            Some(Credential::ApiKey(value)) => request.bearer_auth(value),
            Some(Credential::Header(name, value)) => request.header(name, value),
            Some(Credential::None) | None => request,
        })
        .await
    }

    /// POST a streaming request. A non-success status becomes
    /// [`LlmError::Http`], with the body capped at `MAX_ERROR_BODY` bytes so an
    /// HTML challenge page stays readable.
    pub async fn post_stream(
        &self,
        url: &str,
        body: &str,
        credential: Option<&Credential>,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response, LlmError> {
        let response = self
            .post_with_headers(url, body, credential, headers)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.map_err(LlmError::Transport)?;
            return Err(LlmError::Http {
                status: status.as_u16(),
                body: truncate_bytes(&body, MAX_ERROR_BODY),
            });
        }
        Ok(response)
    }

    async fn send<F>(
        &self,
        url: &str,
        body: &str,
        headers: &[(String, String)],
        configure: F,
    ) -> Result<reqwest::Response, LlmError>
    where
        F: Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    {
        let mut attempt = 0;
        loop {
            let mut request = configure(
                self.client
                    .post(url)
                    .header("Content-Type", "application/json"),
            );
            for (name, value) in headers {
                request = request.header(name, value);
            }

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
