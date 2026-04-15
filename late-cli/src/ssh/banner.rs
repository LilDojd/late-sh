use std::mem;

pub const CLI_TOKEN_PREFIX: &str = "LATE_SESSION_TOKEN=";

#[derive(Debug, PartialEq, Eq)]
enum State {
    Scanning,
    TokenEmitted,
}

#[derive(Debug)]
pub struct BannerParser {
    state: State,
    buf: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Passthrough(Vec<u8>),
    Token(String),
}

impl Default for BannerParser {
    fn default() -> Self {
        Self::new()
    }
}

impl BannerParser {
    pub fn new() -> Self {
        Self {
            state: State::Scanning,
            buf: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();

        if self.state == State::TokenEmitted {
            if !bytes.is_empty() {
                events.push(Event::Passthrough(bytes.to_vec()));
            }
            return events;
        }

        self.buf.extend_from_slice(bytes);

        loop {
            let Some(nl) = self.buf.iter().position(|b| *b == b'\n') else {
                break;
            };

            let consumed = nl + 1;
            let line_bytes: Vec<u8> = self.buf.drain(..consumed).collect();
            let text = String::from_utf8_lossy(&line_bytes);
            if let Some(rest) = text.strip_prefix(CLI_TOKEN_PREFIX) {
                events.push(Event::Token(rest.trim().to_string()));
                self.state = State::TokenEmitted;
                let tail = mem::take(&mut self.buf);
                if !tail.is_empty() {
                    events.push(Event::Passthrough(tail));
                }
                break;
            } else {
                events.push(Event::Passthrough(line_bytes));
            }
        }

        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_token_from_prefixed_line() {
        let mut p = BannerParser::new();
        let evts = p.feed(b"LATE_SESSION_TOKEN=abc-123\r\n");
        assert_eq!(evts, vec![Event::Token("abc-123".into())]);
    }

    #[test]
    fn emits_token_and_tail_after_banner() {
        let mut p = BannerParser::new();
        let evts = p.feed(b"LATE_SESSION_TOKEN=abc-123\r\n\x1b[?1049h");
        assert_eq!(
            evts,
            vec![
                Event::Token("abc-123".into()),
                Event::Passthrough(b"\x1b[?1049h".to_vec()),
            ]
        );
    }

    #[test]
    fn passthrough_lines_before_banner() {
        let mut p = BannerParser::new();
        let evts = p.feed(b"hello\r\nworld");
        assert_eq!(evts, vec![Event::Passthrough(b"hello\r\n".to_vec())]);
    }

    #[test]
    fn buffers_partial_line_until_newline() {
        let mut p = BannerParser::new();
        assert!(p.feed(b"LATE_SESSION_TOK").is_empty());
        let evts = p.feed(b"EN=xyz\r\n");
        assert_eq!(evts, vec![Event::Token("xyz".into())]);
    }

    #[test]
    fn after_token_all_bytes_are_passthrough() {
        let mut p = BannerParser::new();
        p.feed(b"LATE_SESSION_TOKEN=abc\r\n");
        let evts = p.feed(b"some tui output");
        assert_eq!(evts, vec![Event::Passthrough(b"some tui output".to_vec())]);
    }

    #[test]
    fn after_token_later_banner_line_is_not_reparsed() {
        let mut p = BannerParser::new();
        p.feed(b"LATE_SESSION_TOKEN=first\r\n");
        let evts = p.feed(b"LATE_SESSION_TOKEN=second\r\n");
        assert_eq!(
            evts,
            vec![Event::Passthrough(
                b"LATE_SESSION_TOKEN=second\r\n".to_vec()
            )]
        );
    }
}
