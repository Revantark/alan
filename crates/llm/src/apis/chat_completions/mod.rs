mod codec;

use crate::{AssistantMessage, HttpClient, LlmApi, LlmError, LlmRequest};
use async_trait::async_trait;
use std::sync::Arc;

const ENDPOINT: &str = "/chat/completions";

pub struct ChatCompletionsApi {
    base_url: String,
    http: Arc<HttpClient>,
}

impl ChatCompletionsApi {
    pub fn new(base_url: impl Into<String>, http: Arc<HttpClient>) -> Self {
        Self {
            base_url: base_url.into(),
            http,
        }
    }
}

#[async_trait]
impl LlmApi for ChatCompletionsApi {
    async fn complete(&self, request: LlmRequest<'_>) -> Result<AssistantMessage, LlmError> {
        let body = codec::serialize_request(&request)?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), ENDPOINT);
        let response = self.http.post(&url, &body, request.credential).await?;
        let status = response.status();
        let body = response.text().await.map_err(LlmError::Transport)?;
        if !status.is_success() {
            return Err(LlmError::Http {
                status: status.as_u16(),
                body,
            });
        }
        codec::deserialize_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, RequestOptions, ToolDefinition};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn request<'a>(
        model_id: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDefinition],
        options: &'a RequestOptions,
    ) -> LlmRequest<'a> {
        LlmRequest {
            model_id,
            messages,
            tools,
            options,
            credential: None,
        }
    }

    #[tokio::test]
    async fn sends_chat_completion_request() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            let mut content_length = None;
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if content_length.is_none() {
                    if let Some(headers_end) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers = String::from_utf8_lossy(&request[..headers_end]);
                        content_length = headers.lines().find_map(|line| {
                            line.strip_prefix("content-length:")
                                .or_else(|| line.strip_prefix("Content-Length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        });
                    }
                }
                let body_start = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| position + 4);
                if let (Some(body_start), Some(content_length)) = (body_start, content_length)
                    && request.len() >= body_start + content_length
                {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"pong\"}}]}",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let api =
            ChatCompletionsApi::new(format!("http://{address}/v1/"), Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let response = api
            .complete(request("model", &messages, &[], &options))
            .await
            .unwrap();
        assert_eq!(response.text(), "pong");

        let raw_request = server.await.unwrap();
        assert!(raw_request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        let body = raw_request.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "model");
    }
}
