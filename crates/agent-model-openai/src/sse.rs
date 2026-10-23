//! Incremental server-sent-events decoding.
//!
//! The decoder buffers **bytes**, not text: a chunk boundary can fall inside a
//! multi-byte UTF-8 character, so nothing is decoded until a whole line has
//! arrived. Decoding per chunk would mangle non-ASCII text.

/// One assembled `event:`/`data:` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field, when the server sent one.
    pub event: Option<String>,
    /// All `data:` lines of the block, joined with newlines.
    pub data: String,
}

impl SseFrame {
    /// Whether this frame is the OpenAI-compatible end-of-stream sentinel.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// A byte-fed SSE parser. Feed it whatever the transport delivers; it returns
/// the frames that completed.
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: Option<String>,
    data_lines: Vec<String>,
}

impl SseDecoder {
    /// A decoder with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a chunk and return every frame that is now complete.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(idx) = self.buf.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=idx).collect();
            line.pop(); // the trailing '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.consume_line(&line, &mut frames);
        }
        frames
    }

    /// Bytes held back because no newline has arrived yet.
    pub fn pending_bytes(&self) -> usize {
        self.buf.len()
    }

    /// Flush the decoder at end-of-stream.
    ///
    /// A block is only dispatched when its terminating blank line arrives, so a
    /// connection that closes right after the last `data:` line — without the
    /// blank line, or the `[DONE]` sentinel's own trailing blank line — would
    /// silently drop that final frame. At EOF, dispatch whatever complete lines
    /// the decoder is holding.
    ///
    /// A trailing line with no newline is **not** salvaged: the bytes are a
    /// prefix of a frame whose end never arrived, and inventing a frame from
    /// them would turn a truncation into a spurious protocol error. Idempotent —
    /// flushing a decoder that ends on a blank line yields nothing.
    pub fn flush(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        self.dispatch(&mut frames);
        frames
    }

    fn consume_line(&mut self, line: &[u8], frames: &mut Vec<SseFrame>) {
        if line.is_empty() {
            self.dispatch(frames);
            return;
        }
        if line[0] == b':' {
            // A comment line, which is how SSE keep-alives are sent.
            return;
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.find(':') {
            Some(i) => {
                let raw = &text[i + 1..];
                (text[..i].to_string(), raw.strip_prefix(' ').unwrap_or(raw).to_string())
            }
            // A bare field name is valid and means an empty value.
            None => (text.to_string(), String::new()),
        };
        match field.as_str() {
            "event" => self.event = Some(value),
            "data" => self.data_lines.push(value),
            _ => {}
        }
    }

    fn dispatch(&mut self, frames: &mut Vec<SseFrame>) {
        // A block with no data lines carries nothing to deliver; the spec says
        // to drop it rather than emit an empty frame.
        if self.data_lines.is_empty() {
            self.event = None;
            return;
        }
        frames.push(SseFrame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data_lines).join("\n"),
        });
    }
}
