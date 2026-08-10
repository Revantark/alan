use crate::LlmError;

#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseDecoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, LlmError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            self.process_line(&line[..line.len() - 1], &mut events)?;
        }
        Ok(events)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<String>, LlmError> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.process_line(&line, &mut events)?;
        }
        if !self.data_lines.is_empty() {
            events.push(self.take_data());
        }
        Ok(events)
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<String>) -> Result<(), LlmError> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if !self.data_lines.is_empty() {
                events.push(self.take_data());
            }
            return Ok(());
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let Some(data) = line.strip_prefix(b"data:") else {
            return Ok(());
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let data = String::from_utf8(data.to_vec()).map_err(|error| {
            LlmError::InvalidResponse(format!("SSE data is not valid UTF-8: {error}"))
        })?;
        self.data_lines.push(data);
        Ok(())
    }

    fn take_data(&mut self) -> String {
        self.data_lines.drain(..).collect::<Vec<_>>().join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_split_events_and_crlf() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"a\":").unwrap().is_empty());
        assert_eq!(
            decoder.push(b"1}\r\n\r\ndata: [DONE]\r\n\r\n").unwrap(),
            ["{\"a\":1}", "[DONE]"]
        );
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let mut decoder = SseDecoder::default();
        let events = decoder
            .push(b": heartbeat\ndata: first\nretry: 1000\ndata: second\n\n")
            .unwrap();
        assert_eq!(events, ["first\nsecond"]);
    }

    #[test]
    fn accepts_trailing_event_at_eof() {
        let mut decoder = SseDecoder::default();
        decoder.push(b"data: [DONE]").unwrap();
        assert_eq!(decoder.finish().unwrap(), ["[DONE]"]);
    }
}
