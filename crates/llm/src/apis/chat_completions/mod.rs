mod codec;
mod sse;

use crate::{HttpClient, LlmApi, LlmError, LlmEvent, LlmRequest, LlmStream, StopReason};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::collections::VecDeque;
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
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let body = codec::serialize_request(&request)?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), ENDPOINT);
        let response = self.http.post(&url, &body, request.credential).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.map_err(LlmError::Transport)?;
            return Err(LlmError::Http {
                status: status.as_u16(),
                body,
            });
        }

        let state = StreamState::new(response.bytes_stream());
        let output = stream::try_unfold(state, |mut state| async move {
            let event = state.next_event().await?;
            Ok::<_, LlmError>(event.map(|event| (event, state)))
        });

        Ok(Box::pin(output))
    }
}

struct StreamState<S> {
    input: S,
    decoder: sse::SseDecoder,
    pending: VecDeque<LlmEvent>,
    model: Option<String>,
    stop_reason: Option<StopReason>,
    usage: Option<crate::Usage>,
    done: bool,
}

impl<S> StreamState<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    fn new(input: S) -> Self {
        Self {
            input,
            decoder: sse::SseDecoder::default(),
            pending: VecDeque::new(),
            model: None,
            stop_reason: None,
            usage: None,
            done: false,
        }
    }

    async fn next_event(&mut self) -> Result<Option<LlmEvent>, LlmError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            if self.done {
                return Ok(None);
            }

            match self.input.next().await {
                Some(Ok(bytes)) => {
                    let payloads = self.decoder.push(&bytes)?;
                    self.add_payloads(payloads)?;
                }
                Some(Err(error)) => return Err(LlmError::Transport(error)),
                None => {
                    let payloads = self.decoder.finish()?;
                    self.add_payloads(payloads)?;
                    if !self.done {
                        return Err(LlmError::InvalidResponse(
                            "stream ended without [DONE]".into(),
                        ));
                    }
                }
            }
        }
    }

    fn add_payloads(&mut self, payloads: Vec<String>) -> Result<(), LlmError> {
        for payload in payloads {
            if payload == "[DONE]" {
                self.done = true;
                self.pending.push_back(LlmEvent::Done {
                    stop_reason: self.stop_reason.unwrap_or(StopReason::Stop),
                    usage: self.usage.clone(),
                    model: self.model.clone(),
                });
                continue;
            }

            let chunk = codec::deserialize_stream_chunk(&payload)?;
            if let Some(model) = chunk.model.clone() {
                self.model = Some(model);
            }
            if let Some(usage) = &chunk.usage {
                self.usage = Some(usage.clone());
            }
            if let Some(stop_reason) =
                codec::stop_reason_for_finish_reason(chunk.finish_reason.as_deref())
            {
                self.stop_reason = Some(stop_reason);
            }
            // Emit usage *after* the chunk's text/reasoning/tool-call deltas
            // so consumers see content before the summary.
            let usage_event = chunk.usage.clone().map(|usage| LlmEvent::Usage { usage });
            self.pending.extend(codec::stream_events(chunk));
            if let Some(event) = usage_event {
                self.pending.push_back(event);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, RequestOptions, ToolSpec};
    use futures_util::StreamExt;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn request<'a>(
        model_id: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
        options: &'a RequestOptions,
    ) -> LlmRequest<'a> {
        LlmRequest {
            model_id,
            messages,
            tools,
            options,
            credential: None,
            reasoning_effort: None,
        }
    }

    #[tokio::test]
    async fn streams_chat_completion_events() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let body = String::from_utf8_lossy(&request);
                    if body.contains("\"stream\":true") {
                        break;
                    }
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"model\":\"served\",\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let api =
            ChatCompletionsApi::new(format!("http://{address}/v1/"), Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let mut events = api
            .stream(request("model", &messages, &[], &options))
            .await
            .unwrap();
        let mut builder = crate::LlmResponseBuilder::new();
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            builder.apply(&event).unwrap();
        }
        let response = builder.finish().unwrap();
        assert_eq!(response.text(), "hello");
        assert_eq!(response.model.as_deref(), Some("served"));
        assert_eq!(response.usage.unwrap().output_tokens, 3);
        assert!(server.await.unwrap().contains("\"stream\":true"));
    }
}
