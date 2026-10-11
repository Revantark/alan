mod codec;

pub use codec::{
    BaseRequest, BaseResponse, DefaultChatCompletionsCodec, StreamChoice, StreamDelta,
    StreamFunctionWire, StreamToolCallWire, WireDefinition, WireFunction, WireMessage,
    WireReasoning, WireTool, WireToolCall, WireUsage, decode_stream_response,
    stop_reason_for_finish_reason, usage_from_wire,
};

use super::stream::{StreamHandler, sse_events};
use crate::codec::{CodecChunk, LlmApiCodec};
use crate::{HttpClient, LlmApi, LlmError, LlmEvent, LlmRequest, LlmStream, StopReason, Usage};
use async_trait::async_trait;
use std::sync::Arc;

const ENDPOINT: &str = "/chat/completions";

pub struct ChatCompletionsApi {
    base_url: String,
    http: Arc<HttpClient>,
    codec: Arc<dyn LlmApiCodec>,
}

impl ChatCompletionsApi {
    pub fn new(base_url: impl Into<String>, http: Arc<HttpClient>) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            codec: Arc::new(codec::DefaultChatCompletionsCodec),
        }
    }

    /// Attach a provider codec that translates between canonical requests
    /// and this provider's wire format.
    pub fn with_codec(mut self, codec: Arc<dyn LlmApiCodec>) -> Self {
        self.codec = codec;
        self
    }
}

#[async_trait]
impl LlmApi for ChatCompletionsApi {
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let body = self.codec.request(&request)?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), ENDPOINT);
        let response = self
            .http
            .post_stream(&url, &body, request.credential, &[])
            .await?;

        Ok(sse_events(
            response.bytes_stream(),
            ChatHandler::new(self.codec.clone()),
        ))
    }
}

/// Translates canonical `/chat/completions` chunks into [`LlmEvent`]s.
struct ChatHandler {
    codec: Arc<dyn LlmApiCodec>,
    model: Option<String>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
    done: bool,
}

impl ChatHandler {
    fn new(codec: Arc<dyn LlmApiCodec>) -> Self {
        Self {
            codec,
            model: None,
            stop_reason: None,
            usage: None,
            done: false,
        }
    }
}

impl StreamHandler for ChatHandler {
    fn done(&self) -> bool {
        self.done
    }

    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(vec![LlmEvent::Done {
                stop_reason: self.stop_reason.unwrap_or(StopReason::Stop),
                usage: self.usage.clone(),
                model: self.model.clone(),
            }]);
        }

        let chunk: CodecChunk = self.codec.response(data)?;
        if let Some(model) = chunk.model.clone() {
            self.model = Some(model);
        }
        if let Some(usage) = chunk.usage.clone() {
            self.usage = Some(usage);
        }
        if let Some(stop_reason) =
            codec::stop_reason_for_finish_reason(chunk.finish_reason.as_deref())
        {
            self.stop_reason = Some(stop_reason);
        }
        // Emit usage *after* the chunk's text/reasoning/tool-call deltas
        // so consumers see content before the summary.
        let mut events = chunk.events;
        if let Some(usage) = chunk.usage {
            events.push(LlmEvent::Usage { usage });
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.done {
            return Ok(Vec::new());
        }
        Err(LlmError::InvalidResponse(
            "stream ended without [DONE]".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CodecChunk;
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
            reasoning_effort: crate::ReasoningEffort::None,
            extensions: crate::Extensions::default(),
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

    /// A custom codec replaces both directions: the body it serializes is
    /// sent, and the chunk it decodes drives the emitted events.
    #[tokio::test]
    async fn with_codec_uses_the_custom_codec() {
        struct UppercaseCodec;

        impl LlmApiCodec for UppercaseCodec {
            fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
                Ok(format!(
                    r#"{{"model":"{}","upper":true}}"#,
                    request.model_id
                ))
            }

            fn response(&self, _data: &str) -> Result<CodecChunk, LlmError> {
                Ok(CodecChunk {
                    model: Some("rewired".into()),
                    finish_reason: Some("stop".into()),
                    usage: None,
                    events: vec![LlmEvent::TextDelta {
                        text: "decoded by custom codec".into(),
                    }],
                })
            }
        }

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
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: anything\n\ndata: [DONE]\n\n",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let api =
            ChatCompletionsApi::new(format!("http://{address}/v1/"), Arc::new(HttpClient::new()))
                .with_codec(Arc::new(UppercaseCodec));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let mut events = api
            .stream(request("model", &messages, &[], &options))
            .await
            .unwrap();

        let mut texts = Vec::new();
        let mut model = None;
        let mut done = None;
        while let Some(event) = events.next().await {
            match event.unwrap() {
                LlmEvent::TextDelta { text } => texts.push(text),
                LlmEvent::Done { model: served, .. } => model = served,
                other => done = Some(format!("{other:?}")),
            }
        }

        assert_eq!(texts.join(""), "decoded by custom codec");
        assert_eq!(model.as_deref(), Some("rewired"));
        assert_eq!(done, None);
        assert!(server.await.unwrap().contains(r#""upper":true"#));
    }
}
