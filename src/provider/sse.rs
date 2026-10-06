//! Minimal Server-Sent Events decoder over a byte stream.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use super::ProviderError;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE parser. Feed it raw bytes and collect completed events.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<SseEvent>) {
        self.buffer.extend_from_slice(chunk);
        while let Some(pos) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&String::from_utf8_lossy(&line), out);
        }
    }

    /// Flush a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) {
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned();
            self.process_line(line.trim_end_matches('\r'), out);
        }
        self.dispatch(out);
    }

    fn process_line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        out.push(SseEvent { event: self.event.take(), data: self.data.join("\n") });
        self.data.clear();
    }
}

/// Drive `body`, invoking `handle` for each event until the stream ends, `handle` returns
/// `Ok(false)`, or `cancel` fires.
pub async fn for_each_event(
    body: impl Stream<Item = reqwest::Result<Bytes>>,
    cancel: &CancellationToken,
    mut handle: impl FnMut(SseEvent) -> Result<bool, ProviderError>,
) -> Result<(), ProviderError> {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    let mut body = std::pin::pin!(body);
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::fatal("aborted")),
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(bytes)) => parser.push(&bytes, &mut events),
            Some(Err(err)) => {
                return Err(ProviderError::retryable(format!("stream interrupted: {}", super::error_chain(&err))));
            }
            None => {
                parser.finish(&mut events);
                for event in events.drain(..) {
                    if !handle(event)? {
                        return Ok(());
                    }
                }
                return Ok(());
            }
        }
        for event in events.drain(..) {
            if !handle(event)? {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_across_chunks() {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        parser.push(b"event: message_start\ndata: {\"a\":", &mut out);
        assert!(out.is_empty());
        parser.push(b"1}\n\n: comment\ndata: x\r\ndata: y\r\n\r\n", &mut out);
        assert_eq!(
            out,
            vec![
                SseEvent { event: Some("message_start".into()), data: "{\"a\":1}".into() },
                SseEvent { event: None, data: "x\ny".into() },
            ]
        );
    }

    #[test]
    fn finish_flushes_unterminated_event() {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        parser.push(b"data: [DONE]", &mut out);
        parser.finish(&mut out);
        assert_eq!(out, vec![SseEvent { event: None, data: "[DONE]".into() }]);
    }
}
