//! Shared SSE streaming machinery for the API clients.

use super::sse::SseDecoder;
use crate::{LlmError, LlmEvent, LlmStream};
use futures_util::{Stream, StreamExt, stream};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};

/// Interprets the `data:` frames of one provider SSE stream into
/// [`LlmEvent`]s.
///
/// Implemented once per API; [`sse_events`] owns the shared plumbing (SSE
/// decoding, event ordering, end-of-stream handling) so implementations only
/// translate frames.
pub(crate) trait StreamHandler: Send {
    /// Whether the provider's terminal frame has been seen; once true, the
    /// stream stops without reading further frames.
    fn done(&self) -> bool;

    /// Translate one `data:` frame into the events to emit, in order.
    fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError>;

    /// Settle the stream at end of input, after the final frames were passed
    /// to [`StreamHandler::frame`]: return the closing events, or an error
    /// when the protocol requires a terminal frame that never arrived.
    fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError>;
}

/// Decode `input` as an SSE body and emit the events [`StreamHandler`]
/// translates its frames into.
pub(crate) fn sse_events<S, H>(input: S, handler: H) -> LlmStream
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin + Send + 'static,
    H: StreamHandler + 'static,
{
    let state = StreamState {
        input,
        decoder: SseDecoder::default(),
        handler,
        pending: VecDeque::new(),
        finished: false,
    };
    let output = stream::try_unfold(state, |mut state| async move {
        let event = state.next_event().await?;
        Ok::<_, LlmError>(event.map(|event| (event, state)))
    });
    Box::pin(output)
}

/// Decodes SSE frames, hands them to the handler, and buffers the resulting
/// events until the consumer drains them.
struct StreamState<S, H> {
    input: S,
    decoder: SseDecoder,
    handler: H,
    pending: VecDeque<LlmEvent>,
    /// Whether end of input was reached and [`StreamHandler::finish`] ran.
    finished: bool,
}

impl<S, H> StreamState<S, H>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
    H: StreamHandler,
{
    async fn next_event(&mut self) -> Result<Option<LlmEvent>, LlmError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if self.finished || self.handler.done() {
                return Ok(None);
            }
            match self.input.next().await {
                Some(Ok(bytes)) => {
                    for frame in self.decoder.push(&bytes)? {
                        // Frames arriving after the terminal one (e.g. in the
                        // same TCP batch) are dropped: no events may follow
                        // `Done`.
                        if self.handler.done() {
                            break;
                        }
                        self.pending.extend(self.handler.frame(&frame)?);
                    }
                }
                Some(Err(error)) => return Err(LlmError::Transport(error)),
                None => {
                    for frame in self.decoder.finish()? {
                        self.pending.extend(self.handler.frame(&frame)?);
                    }
                    self.finished = true;
                    self.pending.extend(self.handler.finish()?);
                }
            }
        }
    }
}

/// Assigns the dense tool-call indices the response builder expects, keyed by
/// the provider's stream position (`output_index` / step `index`).
#[derive(Default)]
pub(crate) struct ToolIndices {
    positions: BTreeMap<usize, usize>,
    next: usize,
}

impl ToolIndices {
    /// Index for `position`, allocating the next dense index on first sight;
    /// unpositioned calls (`None`) always get a fresh dense index.
    pub(crate) fn index(&mut self, position: Option<usize>) -> usize {
        let Some(position) = position else {
            return self.allocate();
        };
        if let Some(&index) = self.positions.get(&position) {
            return index;
        }
        let index = self.next;
        self.next += 1;
        self.positions.insert(position, index);
        index
    }

    /// Allocate the next dense index for a call the provider did not position.
    pub(crate) fn allocate(&mut self) -> usize {
        let index = self.next;
        self.next += 1;
        index
    }
}

/// Error object carried by provider stream events:
/// `{"code": ..., "message": ...}`.
#[derive(Debug, Deserialize)]
pub(crate) struct WireError {
    #[serde(default)]
    pub(crate) code: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) message: Option<String>,
}

/// Build the stream error for a provider error event, surfacing the error
/// code and the error/event message when present.
pub(crate) fn stream_error(
    api: &str,
    error: Option<WireError>,
    message: Option<String>,
) -> LlmError {
    let mut parts = Vec::new();
    if let Some(error) = &error
        && let Some(code) = error.code.as_ref().and_then(|code| {
            code.as_str()
                .map(str::to_owned)
                .or_else(|| code.as_i64().map(|code| code.to_string()))
        })
    {
        parts.push(format!("code {code}"));
    }
    if let Some(message) = error.and_then(|error| error.message) {
        parts.push(message);
    }
    if let Some(message) = message
        && !message.is_empty()
    {
        parts.push(message);
    }
    LlmError::InvalidResponse(if parts.is_empty() {
        format!("{api} stream failed without an error message")
    } else {
        format!("{api} stream error ({})", parts.join(": "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StopReason;
    use bytes::Bytes;
    use futures_util::StreamExt;

    /// Emits one text event per frame; ends the stream on the terminal frame.
    struct Counting {
        terminal: String,
        done: bool,
    }

    impl StreamHandler for Counting {
        fn done(&self) -> bool {
            self.done
        }

        fn frame(&mut self, data: &str) -> Result<Vec<LlmEvent>, LlmError> {
            if data == self.terminal {
                self.done = true;
                return Ok(vec![LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model: None,
                }]);
            }
            Ok(vec![LlmEvent::TextDelta {
                text: data.to_owned(),
            }])
        }

        fn finish(&mut self) -> Result<Vec<LlmEvent>, LlmError> {
            if self.done {
                return Ok(Vec::new());
            }
            Err(LlmError::InvalidResponse("missing terminal frame".into()))
        }
    }

    fn input(chunks: Vec<&str>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Unpin {
        futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok(Bytes::from(chunk.to_owned())))
                .collect::<Vec<_>>(),
        )
    }

    async fn events(
        chunks: Vec<&'static str>,
        terminal: &'static str,
    ) -> Result<Vec<LlmEvent>, LlmError> {
        let mut events = sse_events(
            input(chunks),
            Counting {
                terminal: terminal.to_owned(),
                done: false,
            },
        );
        let mut collected = Vec::new();
        while let Some(event) = events.next().await {
            collected.push(event?);
        }
        Ok(collected)
    }

    #[tokio::test]
    async fn frames_become_events_in_order() {
        let events = events(vec!["data: a\n\n", "data: b\n\ndata: end\n\n"], "end")
            .await
            .unwrap();
        assert_eq!(
            events,
            vec![
                LlmEvent::TextDelta { text: "a".into() },
                LlmEvent::TextDelta { text: "b".into() },
                LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn frames_after_the_terminal_frame_are_dropped() {
        let events = events(vec!["data: end\n\ndata: late\n\n"], "end")
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], LlmEvent::Done { .. }));
    }

    #[tokio::test]
    async fn missing_terminal_frame_is_an_error() {
        let error = events(vec!["data: a\n\n"], "end").await.unwrap_err();
        assert!(error.to_string().contains("missing terminal frame"));
    }

    #[test]
    fn tool_indices_are_dense_and_stable() {
        let mut indices = ToolIndices::default();
        assert_eq!(indices.index(Some(7)), 0);
        assert_eq!(indices.index(Some(7)), 0);
        assert_eq!(indices.index(Some(2)), 1);
        assert_eq!(indices.index(None), 2);
        assert_eq!(indices.index(None), 3);
        assert_eq!(indices.index(Some(7)), 0);
    }

    #[test]
    fn stream_error_includes_code_and_message() {
        let error = stream_error(
            "test",
            Some(WireError {
                code: Some(serde_json::json!("rate_limit")),
                message: Some("slow down".into()),
            }),
            Some("event message".into()),
        );
        assert_eq!(
            error.to_string(),
            "invalid response: test stream error (code rate_limit: slow down: event message)"
        );

        let numeric = stream_error(
            "test",
            Some(WireError {
                code: Some(serde_json::json!(429)),
                message: None,
            }),
            None,
        );
        assert_eq!(
            numeric.to_string(),
            "invalid response: test stream error (code 429)"
        );

        let empty = stream_error("test", None, None);
        assert_eq!(
            empty.to_string(),
            "invalid response: test stream failed without an error message"
        );
    }
}
