mod codec;

use super::stream::{StreamHandler, ToolIndices, sse_events, stream_error};
use crate::{Credential, HttpClient, LlmApi, LlmError, LlmEvent, LlmRequest, LlmStream, Usage};
use async_trait::async_trait;
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

        let response = self.http.post_stream(&url, &body, credential, &[]).await?;

        Ok(sse_events(
            response.bytes_stream(),
            InteractionsHandler::default(),
        ))
    }
}

/// Translates Gemini Interactions stream events into [`LlmEvent`]s.
#[derive(Default)]
struct InteractionsHandler {
    model: Option<String>,
    status: Option<String>,
    usage: Option<Usage>,
    done: bool,
    /// Whether any content (text/reasoning/tool) was emitted from deltas, used
    /// to decide whether the `interaction.completed` steps fallback is needed.
    emitted_content: bool,
    /// Thought signature streamed before a `function_call` step; attached to
    /// that call so it can be replayed verbatim in the next request.
    pending_signature: Option<String>,
    tool_indices: ToolIndices,
}

impl StreamHandler for InteractionsHandler {
    fn done(&self) -> bool {
        self.done
    }

    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(Vec::new());
        }
        // The stream terminates with a non-JSON `[DONE]` sentinel; complete
        // gracefully if no `interaction.completed` event was seen first.
        if data == "[DONE]" {
            if !self.done {
                let status = self.status.clone().unwrap_or_else(|| "completed".into());
                return Ok(self.complete(&status));
            }
            return Ok(Vec::new());
        }
        // Only object frames carry events; skip stray non-object payloads.
        if !data.starts_with('{') {
            return Ok(Vec::new());
        }
        self.handle_event(codec::deserialize_stream_event(data)?)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.done {
            return Ok(Vec::new());
        }
        if let Some(status) = self.status.clone() {
            return Ok(self.complete(&status));
        }
        Err(LlmError::InvalidResponse(
            "stream ended without completion event".into(),
        ))
    }
}

impl InteractionsHandler {
    fn handle_event(&mut self, event: codec::WireEvent) -> Result<Vec<LlmEvent>, LlmError> {
        match event.event_type.as_str() {
            "step.start" => Ok(self.handle_step_start(event)),
            "step.delta" => Ok(self.handle_step_delta(event)),
            "step.stop" => Ok(self.handle_step_stop(event)),
            "interaction.created" => Ok(self.handle_interaction_created(event)),
            "interaction.completed" => Ok(self.handle_interaction_completed(event)),
            "interaction.status_update" => {
                if let Some(status) = event.status {
                    self.status = Some(status);
                }
                Ok(Vec::new())
            }
            "error" => Err(stream_error("interactions", event.error, None)),
            _ => Ok(Vec::new()),
        }
    }

    fn handle_step_start(&mut self, event: codec::WireEvent) -> Vec<LlmEvent> {
        if let Some(step) = &event.step
            && step.kind == "function_call"
        {
            let index = self.tool_indices.index(event.index);
            self.emitted_content = true;
            let signature = self.pending_signature.take();
            return vec![LlmEvent::ToolCallDelta {
                index,
                id: step.id.clone(),
                name: step.name.clone(),
                arguments: String::new(),
                signature,
            }];
        }
        Vec::new()
    }

    fn handle_step_delta(&mut self, event: codec::WireEvent) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        if let Some(usage) = event.metadata.and_then(|metadata| metadata.total_usage) {
            let usage = codec::usage_from_wire(&usage);
            self.usage = Some(usage.clone());
            events.push(LlmEvent::Usage { usage });
        }

        let Some(delta) = event.delta else {
            return events;
        };
        match delta.kind.as_str() {
            "text" => {
                if let Some(text) = delta.text.filter(|text| !text.is_empty()) {
                    self.emitted_content = true;
                    events.push(LlmEvent::TextDelta { text });
                }
            }
            "thought_summary" => {
                if let Some(text) = delta
                    .content
                    .and_then(|content| content.text)
                    .filter(|text| !text.is_empty())
                {
                    self.emitted_content = true;
                    events.push(LlmEvent::ReasoningDelta {
                        reasoning: text.clone(),
                        details: vec![serde_json::json!({
                            "type": "thought_summary",
                            "summary": text,
                        })],
                    });
                }
            }
            "arguments_delta" => {
                let index = self.tool_indices.index(event.index);
                self.emitted_content = true;
                events.push(LlmEvent::ToolCallDelta {
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
        events
    }

    fn handle_step_stop(&mut self, event: codec::WireEvent) -> Vec<LlmEvent> {
        if let Some(usage) = event.usage {
            let usage = codec::usage_from_wire(&usage);
            self.usage = Some(usage.clone());
            return vec![LlmEvent::Usage { usage }];
        }
        Vec::new()
    }

    fn handle_interaction_created(&mut self, event: codec::WireEvent) -> Vec<LlmEvent> {
        if let Some(interaction) = event.interaction {
            if interaction.model.is_some() {
                self.model = interaction.model;
            }
            if interaction.status.is_some() {
                self.status = interaction.status;
            }
        }
        Vec::new()
    }

    fn handle_interaction_completed(&mut self, event: codec::WireEvent) -> Vec<LlmEvent> {
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

        let mut events = Vec::new();
        if !self.emitted_content
            && let Some(interaction) = &interaction
        {
            self.emit_steps(interaction, &mut events);
        }

        events.extend(self.complete(&status));
        events
    }

    /// Fallback for providers that send a non-streamed body inside
    /// `interaction.completed` instead of `step.delta` frames.
    fn emit_steps(&mut self, interaction: &codec::WireInteraction, events: &mut Vec<LlmEvent>) {
        for step in &interaction.steps {
            match step.kind.as_str() {
                "model_output" => {
                    for content in &step.content {
                        if let Some(text) = content.text.clone().filter(|text| !text.is_empty()) {
                            self.emitted_content = true;
                            events.push(LlmEvent::TextDelta { text });
                        }
                    }
                }
                "function_call" => {
                    let index = self.tool_indices.allocate();
                    self.emitted_content = true;
                    events.push(LlmEvent::ToolCallDelta {
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

    fn complete(&mut self, status: &str) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        if let Some(usage) = &self.usage {
            events.push(LlmEvent::Usage {
                usage: usage.clone(),
            });
        }
        self.done = true;
        self.status = Some(status.to_owned());
        events.push(LlmEvent::Done {
            stop_reason: codec::stop_reason_for_status(Some(status)),
            usage: self.usage.clone(),
            model: self.model.clone(),
        });
        events
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
            extensions: crate::Extensions::default(),
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
