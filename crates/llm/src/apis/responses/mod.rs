mod codec;

pub use codec::{BaseRequest, DefaultResponsesCodec, decode_usage};

use super::stream::{ToolIndices, sse_events, stream_error};
use crate::{
    Headers, HttpClient, LlmApi, LlmApiCodec, LlmError, LlmEvent, LlmRequest, LlmStream,
    LlmStreamDecoder, SessionId, StopReason, Usage,
};
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;

const ENDPOINT: &str = "/responses";

/// OpenAI Responses wire client.
///
/// Each request resends the full history. Use [`Self::with_codec`] for
/// provider-specific wire formats.
pub struct ResponsesApi {
    base_url: String,
    http: Arc<HttpClient>,
    headers: Vec<(String, String)>,
    codec: Arc<dyn LlmApiCodec>,
}

impl ResponsesApi {
    pub fn new(base_url: impl Into<String>, http: Arc<HttpClient>) -> Self {
        Self {
            base_url: base_url.into(),
            http,
            headers: Vec::new(),
            codec: Arc::new(codec::DefaultResponsesCodec),
        }
    }

    /// Attach a static header sent with every request from this instance.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attach a codec that encodes requests and decodes this gateway's stream.
    pub fn with_codec(mut self, codec: Arc<dyn LlmApiCodec>) -> Self {
        self.codec = codec;
        self
    }
}

#[async_trait]
impl LlmApi for ResponsesApi {
    async fn stream(&self, request: LlmRequest<'_>) -> Result<LlmStream, LlmError> {
        let body = self.codec.request(&request)?;
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), ENDPOINT);
        let mut headers = self.headers.clone();
        if let Some(Headers(extra)) = request.extensions.get::<Headers>() {
            headers.extend(extra.iter().cloned());
        }
        // The agent stamps each conversation with a canonical session id;
        // send it as the backend's `session_id` header when present.
        if let Some(session) = request.extensions.get::<SessionId>()
            && !session.0.is_empty()
        {
            headers.push(("session_id".to_string(), session.0.clone()));
        }
        let response = self
            .http
            .post_stream(&url, &body, request.credential, &headers)
            .await?;

        Ok(sse_events(response.bytes_stream(), self.codec.decoder()))
    }
}

/// Decodes Responses events, tracking tool calls and stream completion.
pub struct ResponsesDecoder {
    decode_usage: fn(&serde_json::Value) -> Result<Usage, LlmError>,
    model: Option<String>,
    usage: Option<Usage>,
    done: bool,
    /// Output indexes whose function-call arguments were already emitted, so
    /// a trailing `...arguments.done` does not duplicate them.
    emitted_arguments: HashSet<usize>,
    tool_indices: ToolIndices,
}

impl Default for ResponsesDecoder {
    fn default() -> Self {
        Self::with_usage_decoder(codec::decode_usage)
    }
}

impl ResponsesDecoder {
    /// Override usage mapping for both `Usage` and `Done` events.
    pub fn with_usage_decoder(
        decode_usage: fn(&serde_json::Value) -> Result<Usage, LlmError>,
    ) -> Self {
        Self {
            decode_usage,
            model: None,
            usage: None,
            done: false,
            emitted_arguments: HashSet::new(),
            tool_indices: ToolIndices::default(),
        }
    }
}

impl LlmStreamDecoder for ResponsesDecoder {
    fn done(&self) -> bool {
        self.done
    }

    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
        let data = data.trim();
        if data.is_empty() || !data.starts_with('{') {
            return Ok(Vec::new());
        }
        self.handle_event(codec::deserialize_stream_event(data)?)
    }

    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
        if self.done {
            Ok(Vec::new())
        } else {
            Err(LlmError::InvalidResponse(
                "stream ended without a terminal event".into(),
            ))
        }
    }
}

impl ResponsesDecoder {
    fn handle_event(&mut self, event: codec::WireEvent) -> Result<Vec<LlmEvent>, LlmError> {
        let mut events = Vec::new();
        match event.kind.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(model) = event.response.and_then(|response| response.model) {
                    self.model = Some(model);
                }
            }
            "response.output_text.delta" => {
                if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                    events.push(LlmEvent::TextDelta { text });
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(text) = event.delta.filter(|text| !text.is_empty()) {
                    events.push(LlmEvent::ReasoningDelta {
                        reasoning: text,
                        details: Vec::new(),
                    });
                }
            }
            "response.output_item.added" => self.handle_item_added(event, &mut events),
            "response.function_call_arguments.delta" => {
                if let (Some(output_index), Some(delta)) = (event.output_index, event.delta)
                    && !delta.is_empty()
                {
                    let index = self.tool_indices.index(Some(output_index));
                    // Keyed by the raw wire `output_index`, matching the
                    // `...arguments.done` check below — not the dense index
                    // this stream emits to the builder.
                    self.emitted_arguments.insert(output_index);
                    events.push(LlmEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments: delta,
                        signature: None,
                    });
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(output_index), Some(arguments)) = (event.output_index, event.arguments)
                    && !arguments.is_empty()
                    && !self.emitted_arguments.contains(&output_index)
                {
                    let index = self.tool_indices.index(Some(output_index));
                    events.push(LlmEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments,
                        signature: None,
                    });
                }
            }
            "response.output_item.done" => self.handle_item_done(event, &mut events),
            "response.completed" => {
                let response = event.response.unwrap_or_default();
                self.complete(response, StopReason::Stop, &mut events)?;
            }
            "response.incomplete" => {
                let response = event.response.unwrap_or_default();
                self.complete(response, StopReason::Length, &mut events)?;
            }
            "response.failed" => {
                let response = event.response.unwrap_or_default();
                let error = response.error.or(event.error);
                return Err(stream_error("responses", error, event.message));
            }
            "error" => return Err(stream_error("responses", event.error, event.message)),
            other => tracing::debug!(event = other, "ignoring responses stream event"),
        }
        Ok(events)
    }

    fn handle_item_added(&mut self, event: codec::WireEvent, events: &mut Vec<LlmEvent>) {
        let Some(output_index) = event.output_index else {
            return;
        };
        let Some(item) = event.item else {
            return;
        };
        if item.get("type").and_then(serde_json::Value::as_str) != Some("function_call") {
            return;
        }
        let index = self.tool_indices.index(Some(output_index));
        let id = item
            .get("call_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| item.get("id").and_then(serde_json::Value::as_str))
            .map(str::to_owned);
        let name = item
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        // Non-streamed items may carry the full arguments on the item itself.
        let arguments = item
            .get("arguments")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !arguments.is_empty() {
            self.emitted_arguments.insert(output_index);
        }
        events.push(LlmEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
            signature: None,
        });
    }

    fn handle_item_done(&mut self, event: codec::WireEvent, events: &mut Vec<LlmEvent>) {
        let Some(item) = event.item else {
            return;
        };
        if item.get("type").and_then(serde_json::Value::as_str) != Some("reasoning") {
            return;
        }
        // Keep the full reasoning item (including `encrypted_content`) so the
        // agent can replay it verbatim on the next request.
        events.push(LlmEvent::ReasoningDelta {
            reasoning: String::new(),
            details: vec![item],
        });
    }

    fn complete(
        &mut self,
        response: codec::WireResponse,
        fallback: StopReason,
        events: &mut Vec<LlmEvent>,
    ) -> Result<(), LlmError> {
        let usage = response.usage.as_ref().map(self.decode_usage).transpose()?;
        if response.model.is_some() {
            self.model = response.model;
        }
        let stop_reason = if response.output.iter().any(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
        }) {
            StopReason::ToolUse
        } else {
            fallback
        };

        if let Some(usage) = &usage {
            self.usage = Some(usage.clone());
            events.push(LlmEvent::Usage {
                usage: usage.clone(),
            });
        }
        self.done = true;
        events.push(LlmEvent::Done {
            stop_reason,
            usage: self.usage.clone(),
            model: self.model.clone(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, RequestOptions, StopReason};
    use futures_util::StreamExt;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn user_request<'a>(
        messages: &'a [Message],
        options: &'a RequestOptions,
        credential: &'a crate::Credential,
    ) -> LlmRequest<'a> {
        LlmRequest {
            model_id: "gpt-test",
            messages,
            tools: &[],
            options,
            credential: Some(credential),
            reasoning_effort: crate::ReasoningEffort::None,
            extensions: crate::Extensions::default(),
        }
    }

    fn user_request_with_headers<'a>(
        messages: &'a [Message],
        options: &'a RequestOptions,
        credential: &'a crate::Credential,
        headers: Vec<(String, String)>,
    ) -> LlmRequest<'a> {
        let mut extensions = crate::Extensions::default();
        extensions.insert(crate::Headers(headers));
        LlmRequest {
            model_id: "gpt-test",
            messages,
            tools: &[],
            options,
            credential: Some(credential),
            reasoning_effort: crate::ReasoningEffort::None,
            extensions,
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
                    && String::from_utf8_lossy(&request).contains("\"store\":false")
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
        (format!("http://{address}"), server)
    }

    async fn collect(api: &ResponsesApi, request: LlmRequest<'_>) -> crate::LlmResponse {
        let mut events = api.stream(request).await.unwrap();
        let mut builder = crate::LlmResponseBuilder::new();
        while let Some(event) = events.next().await {
            builder.apply(&event.unwrap()).unwrap();
        }
        builder.finish().unwrap()
    }

    /// Like [`collect`], but expects and returns the stream's error instead of
    /// a response.
    async fn collect_err(api: &ResponsesApi, request: LlmRequest<'_>) -> LlmError {
        let mut events = match api.stream(request).await {
            Ok(events) => events,
            Err(error) => return error,
        };
        let mut builder = crate::LlmResponseBuilder::new();
        while let Some(event) = events.next().await {
            match event {
                Ok(event) => {
                    if let Err(error) = builder.apply(&event) {
                        return error;
                    }
                }
                Err(error) => return error,
            }
        }
        builder
            .finish()
            .expect_err("stream without a terminal event must not complete")
    }

    #[tokio::test]
    async fn with_codec_replaces_both_directions() {
        struct GatewayCodec;

        #[derive(Default)]
        struct GatewayDecoder {
            done: bool,
        }

        impl LlmApiCodec for GatewayCodec {
            fn request(&self, request: &LlmRequest<'_>) -> Result<String, LlmError> {
                let mut body = serde_json::to_value(BaseRequest::new(request)?)
                    .map_err(LlmError::Serialization)?;
                body["gateway"] = true.into();
                serde_json::to_string(&body).map_err(LlmError::Serialization)
            }

            fn decoder(&self) -> Box<dyn LlmStreamDecoder> {
                Box::new(GatewayDecoder::default())
            }
        }

        impl LlmStreamDecoder for GatewayDecoder {
            fn done(&self) -> bool {
                self.done
            }

            fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
                assert_eq!(data, "gateway-payload");
                self.done = true;
                Ok(vec![
                    LlmEvent::TextDelta {
                        text: "custom decoder".into(),
                    },
                    LlmEvent::Done {
                        stop_reason: StopReason::Stop,
                        usage: None,
                        model: Some("gateway-model".into()),
                    },
                ])
            }

            fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
                assert!(self.done);
                Ok(Vec::new())
            }
        }

        let (url, server) = serve("data: gateway-payload\n\n").await;
        let api =
            ResponsesApi::new(url, Arc::new(HttpClient::new())).with_codec(Arc::new(GatewayCodec));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;
        assert_eq!(response.text(), "custom decoder");
        assert_eq!(response.model.as_deref(), Some("gateway-model"));
        let sent = server.await.unwrap();
        assert!(sent.contains("POST /responses"));
        assert!(sent.contains("\"gateway\":true"));
    }

    #[test]
    fn default_codec_decoders_have_independent_stream_state() {
        let codec = DefaultResponsesCodec;
        let mut first = codec.decoder();
        let mut second = codec.decoder();
        first.frame(
            r#"{"type":"response.output_item.added","output_index":7,"item":{"type":"function_call","call_id":"a","name":"read"}}"#,
        ).unwrap();
        first.frame(
            r#"{"type":"response.function_call_arguments.delta","output_index":7,"delta":"{}"}"#,
        ).unwrap();
        let events = second.frame(
            r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","call_id":"b","name":"write"}}"#,
        ).unwrap();
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::ToolCallDelta { index: 0, .. }]
        ));
        let events = second.frame(
            r#"{"type":"response.function_call_arguments.done","output_index":7,"arguments":"{}"}"#,
        ).unwrap();
        assert!(matches!(events.as_slice(), [LlmEvent::ToolCallDelta {
            index: 1, arguments, ..
        }] if arguments == "{}"));
        first
            .frame(r#"{"type":"response.completed","response":{"model":"first"}}"#)
            .unwrap();
        assert!(first.done());
        assert!(!second.done());
        assert!(second.finish().is_err());
        let events = second
            .frame(r#"{"type":"response.completed","response":{}}"#)
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [LlmEvent::Done {
                model: None,
                usage: None,
                ..
            }]
        ));
    }

    #[test]
    fn base_usage_decoder_errors_do_not_complete_the_stream() {
        let mut decoder = DefaultResponsesCodec.decoder();
        let error = decoder
            .frame(
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":"invalid"}}}"#,
            )
            .unwrap_err();
        assert!(matches!(error, LlmError::Serialization(_)));
        assert!(!decoder.done());
    }

    #[test]
    fn base_usage_decoder_ignores_provider_fields() {
        let mut decoder = DefaultResponsesCodec.decoder();
        let events = decoder.frame(
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":2,"output_tokens":3,"cost":0.0021,"cost_details":{"upstream_inference_cost":0.0019}}}}"#,
        ).unwrap();
        let [
            LlmEvent::Usage { usage },
            LlmEvent::Done {
                usage: Some(done_usage),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("expected usage and completion events");
        };
        assert_eq!(usage, done_usage);
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cost, None);
        assert_eq!(usage.upstream_inference_cost, None);
    }

    #[tokio::test]
    async fn streams_text_reasoning_and_usage() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-served\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-served\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":1},\"output_tokens_details\":{\"reasoning_tokens\":7}}}}\n\n",
        );
        let (url, server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        assert_eq!(response.text(), "hello");
        assert_eq!(response.reasoning.as_deref(), Some("thinking"));
        assert_eq!(response.model.as_deref(), Some("gpt-served"));
        assert_eq!(response.stop_reason, StopReason::Stop);
        let usage = response.usage.unwrap();
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.cached_tokens, Some(1));
        assert_eq!(usage.reasoning_tokens, Some(7));

        let sent = server.await.unwrap();
        // The bearer token and content type must be present; provider-specific
        // headers are attached per instance, not by the bare client.
        assert!(sent.contains("authorization: Bearer token"));
        assert!(sent.contains("content-type: application/json"));
    }

    #[tokio::test]
    async fn sends_instance_headers_verbatim() {
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let (url, server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()))
            .with_header("ChatGPT-Account-Id", "acct-1")
            .with_header("OpenAI-Beta", "responses=experimental")
            .with_header("originator", "alan")
            .with_header("session_id", "11111111-1111-1111-1111-111111111111")
            .with_header("version", "0.1.0");
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        collect(&api, user_request(&messages, &options, &credential)).await;

        let sent = server.await.unwrap();
        assert!(sent.contains("chatgpt-account-id: acct-1"));
        assert!(sent.contains("openai-beta: responses=experimental"));
        assert!(sent.contains("originator: alan"));
        assert!(sent.contains("session_id: 11111111-1111-1111-1111-111111111111"));
        assert!(sent.contains("version: 0.1.0"));
    }

    #[tokio::test]
    async fn error_bodies_are_truncated() {
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
            let html = "<html>".repeat(400);
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
                html.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let api = ResponsesApi::new(format!("http://{address}"), Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let error = match api
            .stream(user_request(&messages, &options, &credential))
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("403 must produce an error"),
        };
        server.await.unwrap();

        let rendered = error.to_string();
        assert!(rendered.contains("403"));
        assert!(rendered.contains("[truncated]"));
        // Bounded, not a wall of HTML.
        assert!(
            rendered.len() < 1000,
            "error message too long: {}",
            rendered.len()
        );
    }

    #[tokio::test]
    async fn request_headers_merge_with_instance_headers() {
        let body = "data: {\"type\":\"response.completed\",\"response\":{}}\n\n";
        let (url, server) = serve(body).await;
        let api =
            ResponsesApi::new(url, Arc::new(HttpClient::new())).with_header("originator", "alan");
        // The account id is unknown at construction; auth attaches it per
        // request through the `Headers` extension.
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let request = user_request_with_headers(
            &messages,
            &options,
            &credential,
            vec![("ChatGPT-Account-Id".to_string(), "acct-9".to_string())],
        );
        collect(&api, request).await;

        let sent = server.await.unwrap();
        assert!(sent.contains("chatgpt-account-id: acct-9"));
        assert!(sent.contains("originator: alan"));
    }

    #[tokio::test]
    async fn streams_function_call_and_sets_tool_use() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"bash\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"comm\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"and\\\":\\\"ls\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\"}]}}\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("run it")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{\"command\":\"ls\"}");
        assert_eq!(response.stop_reason, StopReason::ToolUse);
    }

    #[tokio::test]
    async fn streamed_arguments_behind_a_reasoning_item_are_not_duplicated() {
        // Real-session layout: the reasoning item occupies output index 0,
        // so the function call sits at index 1. The `...arguments.done`
        // event re-sends the full arguments; the emitted-arguments guard
        // must key on the wire index, not the dense builder index, or the
        // arguments get appended twice and the JSON gains trailing
        // characters.
        let body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[],\"encrypted_content\":\"ENC\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"\\\"main.rs\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":1,\"arguments\":\"{\\\"path\\\":\\\"main.rs\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\"},{\"type\":\"function_call\"}]}}\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("read the file")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        // Exactly once — a second copy would break tool JSON parsing with
        // "trailing characters".
        assert_eq!(calls[0].arguments, "{\"path\":\"main.rs\"}");
        assert_eq!(response.reasoning_details.len(), 1);
    }

    #[tokio::test]
    async fn non_streamed_function_call_arguments_use_done_event() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"bash\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\"}]}}\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("run it")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        let calls: Vec<_> = response.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, "{\"command\":\"ls\"}");
    }

    #[tokio::test]
    async fn reasoning_items_are_kept_for_replay() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[],\"encrypted_content\":\"ENC\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        assert_eq!(response.reasoning_details.len(), 1);
        assert_eq!(response.reasoning_details[0]["type"], "reasoning");
        assert_eq!(response.reasoning_details[0]["encrypted_content"], "ENC");
        assert_eq!(response.reasoning_details[0]["id"], "rs_1");
    }

    #[tokio::test]
    async fn incomplete_maps_to_length_stop_reason() {
        let body = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        assert_eq!(response.text(), "partial");
        assert_eq!(response.stop_reason, StopReason::Length);
    }

    #[tokio::test]
    async fn failed_event_becomes_an_error() {
        let body = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"boom\"}}}\n\n";
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let error = collect_err(&api, user_request(&messages, &options, &credential)).await;

        assert!(error.to_string().contains("server_error"));
        assert!(error.to_string().contains("boom"));
    }

    #[tokio::test]
    async fn top_level_error_event_becomes_an_error() {
        let body = "data: {\"type\":\"error\",\"message\":\"rate limited\"}\n\n";
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let error = collect_err(&api, user_request(&messages, &options, &credential)).await;

        assert!(error.to_string().contains("rate limited"));
    }

    #[tokio::test]
    async fn stream_without_terminal_event_is_an_error() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"half\"}\n\n";
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let error = collect_err(&api, user_request(&messages, &options, &credential)).await;

        assert!(error.to_string().contains("terminal event"));
    }

    #[tokio::test]
    async fn unknown_event_types_are_ignored() {
        let body = concat!(
            "data: {\"type\":\"response.some_future_event\",\"payload\":{\"x\":1}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        );
        let (url, _server) = serve(body).await;
        let api = ResponsesApi::new(url, Arc::new(HttpClient::new()));
        let messages = [Message::user("ping")];
        let options = RequestOptions::default();
        let credential = crate::Credential::ApiKey("token".into());
        let response = collect(&api, user_request(&messages, &options, &credential)).await;

        assert_eq!(response.stop_reason, StopReason::Stop);
    }
}
