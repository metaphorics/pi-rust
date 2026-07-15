//! Minimal incremental Server-Sent Events parser.
//!
//! Provider streams only use `data` fields. Keeping this parser local avoids an
//! event-source dependency and, importantly, lets tests feed arbitrarily split
//! byte chunks just like a real HTTP body.

#[derive(Debug, Default)]
pub struct SseParser {
    pending: String,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.push_str(&String::from_utf8_lossy(bytes));
        let mut events = Vec::new();
        while let Some(newline) = self.pending.find('\n') {
            let mut line = self.pending[..newline].to_owned();
            self.pending.drain(..=newline);
            if line.ends_with('\r') {
                line.pop();
            }
            self.process_line(&line, &mut events);
        }
        events
    }

    pub fn finish(mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.process_line(&line, &mut events);
        }
        self.emit(&mut events);
        events
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            self.emit(events);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
    }

    fn emit(&mut self, events: &mut Vec<String>) {
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
        }
    }
}

pub fn parse_sse_chunks<I, B>(chunks: I) -> Vec<String>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(chunk.as_ref()));
    }
    events.extend(parser.finish());
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiline_data_across_chunk_boundaries() {
        let chunks = [
            b": ping\r\ndata: {\"a\":".as_slice(),
            b"1}\r\ndata: tail\r\n\r\nid: ignored\r\ndata: done\r\n".as_slice(),
        ];
        assert_eq!(parse_sse_chunks(chunks), ["{\"a\":1}\ntail", "done"]);
    }
}
