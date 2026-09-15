mod codec;

use crate::apis::sse::SseDecoder;
use crate::{Credential, HttpClient, LlmApi, LlmError, LlmEvent, LlmRequest, LlmStream, Usage};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

const ENDPOINT: &str = "/interactions";

/// Gemini Interactions API client (`v1beta/interactions`). Authenticates with
/// the `x-goog-api-key` header rather than bearer tokens.
pub struct InteractionsApi {
    base_url: String,
    http: Arc<HttpClient>,
}

impl InteractionsApi {
    pub fn new(base_url: impl Into<String>, http: Arc<HttpClient>) -> Self {
        Self {
            base_url: base_url.into(),
            http,
        }
    }
}

#[async_trait]
impl LlmApi for InteractionsApi {
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let body = codec::serialize_request(&request)?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), ENDPOINT);
        // For google, we need X-goog-api-key
        // So we shall convert ApiKey from request.credential into an
        // Credential::Header(..)
        let credential = match request.credential {
            Some(Credential::ApiKey(key)) => Some(&Credential::Header(
                "X-goog-api-key".to_string(),
                key.to_string(),
            )),
            other => other,
        };

        let response = self.http.post(&url, &body, credential).await?;
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
    decoder: SseDecoder,
    pending: VecDeque<LlmEvent>,
    model: Option<String>,
    status: Option<String>,
    usage: Option<Usage>,
    done: bool,
    /// Maps a `function_call` step index to the dense tool-call index the
    /// response builder expects.
    tool_indices: BTreeMap<usize, usize>,
    next_tool_index: usize,
    /// Whether any content (text/reasoning/tool) was emitted from deltas, used
    /// to decide whether the `interaction.completed` steps fallback is needed.
    emitted_content: bool,
    /// Thought signature streamed before a `function_call` step; attached to
    /// that call so it can be replayed verbatim in the next request.
    pending_signature: Option<String>,
}

impl<S> StreamState<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    fn new(input: S) -> Self {
        Self {
            input,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            model: None,
            status: None,
            usage: None,
            done: false,
            tool_indices: BTreeMap::new(),
            next_tool_index: 0,
            emitted_content: false,
            pending_signature: None,
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
                        if let Some(status) = self.status.clone() {
                            self.complete(&status);
                        } else {
                            return Err(LlmError::InvalidResponse(
                                "stream ended without completion event".into(),
                            ));
                        }
                    }
                }
            }
        }
    }

    fn add_payloads(&mut self, payloads: Vec<String>) -> Result<(), LlmError> {
        for payload in payloads {
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            // The stream terminates with a non-JSON `[DONE]` sentinel; complete
            // gracefully if no `interaction.completed` event was seen first.
            if payload == "[DONE]" {
                if !self.done {
                    let status = self.status.clone().unwrap_or_else(|| "completed".into());
                    self.complete(&status);
                }
                continue;
            }
            // Only object frames carry events; skip stray non-object payloads.
            if !payload.starts_with('{') {
                continue;
            }
            let event = codec::deserialize_stream_event(payload)?;
            self.handle_event(event)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: codec::WireEvent) -> Result<(), LlmError> {
        match event.event_type.as_str() {
            "step.start" => self.handle_step_start(event),
            "step.delta" => self.handle_step_delta(event),
            "step.stop" => self.handle_step_stop(event),
            "interaction.created" => self.handle_interaction_created(event),
            "interaction.completed" => self.handle_interaction_completed(event),
            "interaction.status_update" => {
                if let Some(status) = event.status {
                    self.status = Some(status);
                }
            }
            "error" => return Err(self.error_for(event.error)),
            _ => {}
        }
        Ok(())
    }

    fn handle_step_start(&mut self, event: codec::WireEvent) {
        if let Some(step) = &event.step
            && step.kind == "function_call"
        {
            let index = self.tool_index(event.index.unwrap_or(self.next_tool_index));
            self.emitted_content = true;
            let signature = self.pending_signature.take();
            self.pending.push_back(LlmEvent::ToolCallDelta {
                index,
                id: step.id.clone(),
                name: step.name.clone(),
                arguments: String::new(),
                signature,
            });
        }
    }

    fn handle_step_delta(&mut self, event: codec::WireEvent) {
        if let Some(usage) = event.metadata.and_then(|metadata| metadata.total_usage) {
            let usage = codec::usage_from_wire(&usage);
            self.usage = Some(usage.clone());
            self.pending.push_back(LlmEvent::Usage { usage });
        }

        let Some(delta) = event.delta else {
            return;
        };
        match delta.kind.as_str() {
            "text" => {
                if let Some(text) = delta.text.filter(|text| !text.is_empty()) {
                    self.emitted_content = true;
                    self.pending.push_back(LlmEvent::TextDelta { text });
                }
            }
            "thought_summary" => {
                if let Some(text) = delta
                    .content
                    .and_then(|content| content.text)
                    .filter(|text| !text.is_empty())
                {
                    self.emitted_content = true;
                    self.pending.push_back(LlmEvent::ReasoningDelta {
                        reasoning: text.clone(),
                        details: vec![serde_json::json!({
                            "type": "thought_summary",
                            "summary": text,
                        })],
                    });
                }
            }
            "arguments_delta" => {
                let index = self.tool_index(event.index.unwrap_or(self.next_tool_index));
                self.emitted_content = true;
                self.pending.push_back(LlmEvent::ToolCallDelta {
                    index,
                    id: None,
                    name: None,
                    arguments: delta.arguments.unwrap_or_default(),
                    signature: None,
                });
            }
            "thought_signature" => {
                if let Some(signature) = delta.signature.clone() {
                    self.pending_signature = Some(signature);
                }
            }
            _ => {}
        }
    }

    fn handle_step_stop(&mut self, event: codec::WireEvent) {
        if let Some(usage) = event.usage {
            let usage = codec::usage_from_wire(&usage);
            self.usage = Some(usage.clone());
            self.pending.push_back(LlmEvent::Usage { usage });
        }
    }

    fn handle_interaction_created(&mut self, event: codec::WireEvent) {
        if let Some(interaction) = event.interaction {
            if interaction.model.is_some() {
                self.model = interaction.model;
            }
            if interaction.status.is_some() {
                self.status = interaction.status;
            }
        }
    }

    fn handle_interaction_completed(&mut self, event: codec::WireEvent) {
        let interaction = event.interaction;
        let status = interaction
            .as_ref()
            .and_then(|interaction| interaction.status.clone())
            .or_else(|| self.status.clone())
            .unwrap_or_else(|| "completed".into());

        if let Some(interaction) = &interaction {
            if interaction.model.is_some() {
                self.model = interaction.model.clone();
            }
            if let Some(usage) = &interaction.usage {
                self.usage = Some(codec::usage_from_wire(usage));
            }
        }

        if !self.emitted_content
            && let Some(interaction) = &interaction
        {
            self.emit_steps(interaction);
        }

        self.complete(&status);
    }

    /// Fallback for providers that send a non-streamed body inside
    /// `interaction.completed` instead of `step.delta` frames.
    fn emit_steps(&mut self, interaction: &codec::WireInteraction) {
        for step in &interaction.steps {
            match step.kind.as_str() {
                "model_output" => {
                    for content in &step.content {
                        if let Some(text) = content.text.clone().filter(|text| !text.is_empty()) {
                            self.emitted_content = true;
                            self.pending.push_back(LlmEvent::TextDelta { text });
                        }
                    }
                }
                "function_call" => {
                    let index = self.tool_index(self.next_tool_index);
                    self.emitted_content = true;
                    self.pending.push_back(LlmEvent::ToolCallDelta {
                        index,
                        id: step.id.clone(),
                        name: step.name.clone(),
                        arguments: step
                            .arguments
                            .as_ref()
                            .map(|arguments| arguments.to_string())
                            .unwrap_or_default(),
                        signature: step.signature.clone(),
                    });
                }
                _ => {}
            }
        }
    }

    fn complete(&mut self, status: &str) {
        if let Some(usage) = &self.usage {
            self.pending.push_back(LlmEvent::Usage {
                usage: usage.clone(),
            });
        }
        self.done = true;
        self.status = Some(status.to_owned());
        self.pending.push_back(LlmEvent::Done {
            stop_reason: codec::stop_reason_for_status(Some(status)),
            usage: self.usage.clone(),
            model: self.model.clone(),
        });
    }

    fn tool_index(&mut self, step_index: usize) -> usize {
        if let Some(index) = self.tool_indices.get(&step_index) {
            return *index;
        }
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.tool_indices.insert(step_index, index);
        index
    }

    fn error_for(&self, error: Option<codec::WireError>) -> LlmError {
        let (code, message) = match error {
            Some(error) => (
                error
                    .code
                    .as_ref()
                    .and_then(|code| code.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "error".into()),
                error.message.unwrap_or_default(),
            ),
            None => ("error".into(), String::new()),
        };
        LlmError::InvalidResponse(format!("interactions stream error ({code}): {message}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, RequestOptions, StopReason, ToolSpec};
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
            reasoning_effort: crate::ReasoningEffort::None,
            provider_order: None,
        }
    }

    async fn serve(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
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
                if request.windows(4).any(|window| window == b"\r\n\r\n")
                    && String::from_utf8_lossy(&request).contains("\"stream\":true")
                {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/v1beta"), server)
    }

    async fn collect(api: &InteractionsApi, request: LlmRequest<'_>) -> crate::LlmResponse {
        let mut events = api.stream(request).await.unwrap();
        let mut builder = crate::LlmResponseBuilder::new();
        while let Some(event) = events.next().await {
            builder.apply(&event.unwrap()).unwrap();
        }
        builder.finish().unwrap()
    }

    #[tokio::test]
    async fn streams_text_and_usage() {
        let body = concat!(
            "data: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"i1\",\"model\":\"served\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"model_output\"}}\n\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hel\"}}\n\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"lo\"}}\n\n",
            "data: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
            "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"model\":\"served\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":2,\"total_output_tokens\":3}}}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, server) = serve(body).await;
        let api = InteractionsApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let response = collect(
            &api,
            request("gemini", &messages, &[], &RequestOptions::default()),
        )
        .await;

        assert_eq!(response.text(), "hello");
        assert_eq!(response.model.as_deref(), Some("served"));
        assert_eq!(response.usage.unwrap().output_tokens, 3);
        assert_eq!(response.stop_reason, StopReason::Stop);
        assert!(server.await.unwrap().contains("\"stream\":true"));
    }

    #[tokio::test]
    async fn streams_function_call_arguments() {
        let body = concat!(
            "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"function_call\",\"name\":\"weather\",\"id\":\"call-1\"}}\n\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"{\\\"city\\\":\"}}\n\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"arguments_delta\",\"arguments\":\"\\\"Paris\\\"}\"}}\n\n",
            "data: {\"event_type\":\"step.stop\",\"index\":0}\n\n",
            "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"model\":\"served\",\"status\":\"requires_action\"}}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = InteractionsApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("weather?")];
        let response = collect(
            &api,
            request("gemini", &messages, &[], &RequestOptions::default()),
        )
        .await;

        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].name, "weather");
        assert_eq!(calls[0].arguments, "{\"city\":\"Paris\"}");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn falls_back_to_completed_steps() {
        let body = "data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"i1\",\"model\":\"served\",\"status\":\"completed\",\"steps\":[{\"type\":\"model_output\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}]}}\n\n";
        let (url, _server) = serve(body).await;
        let api = InteractionsApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let response = collect(
            &api,
            request("gemini", &messages, &[], &RequestOptions::default()),
        )
        .await;
        assert_eq!(response.text(), "done");
    }

    #[tokio::test]
    async fn ignores_done_sentinel_without_completed_event() {
        let body = concat!(
            "data: {\"event_type\":\"step.start\",\"index\":0,\"step\":{\"type\":\"model_output\"}}\n\n",
            "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hi\"}}\n\n",
            "data: [DONE]\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = InteractionsApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let response = collect(
            &api,
            request("gemini", &messages, &[], &RequestOptions::default()),
        )
        .await;
        assert_eq!(response.text(), "hi");
        assert_eq!(response.stop_reason, StopReason::Stop);
    }
}
