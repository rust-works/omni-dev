//! Splits a leading inline `<think>…</think>` block out of model output.
//!
//! Most reasoning models served over the OpenAI protocol return reasoning in a
//! separate `reasoning_content` field, but some (and some servers) inline it at
//! the start of `content`. The splitter only acts on a `<think>` that opens the
//! content (after whitespace), so a `<think>` mentioned later in an answer is
//! left alone, and it works across arbitrary chunk boundaries for streaming.

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// A piece of split output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Reasoning text.
    Reasoning(String),
    /// Answer text.
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Deciding whether the content opens with `<think>`.
    Start,
    /// Inside the think block.
    Thinking,
    /// After `</think>`, dropping the whitespace that separates the answer.
    AfterThink,
    /// Plain answer text.
    Text,
}

/// The streaming splitter.
#[derive(Debug, Clone)]
pub(crate) struct ThinkTagSplitter {
    state: State,
    buf: String,
}

impl Default for ThinkTagSplitter {
    fn default() -> Self {
        Self {
            state: State::Start,
            buf: String::new(),
        }
    }
}

impl ThinkTagSplitter {
    /// Feeds the next content chunk.
    pub(crate) fn push(&mut self, chunk: &str) -> Vec<Piece> {
        let mut out = Vec::new();
        self.buf.push_str(chunk);
        loop {
            match self.state {
                State::Start => {
                    let trimmed = self.buf.trim_start();
                    if trimmed.len() >= OPEN.len() {
                        if let Some(rest) = trimmed.strip_prefix(OPEN) {
                            self.buf = rest.to_string();
                            self.state = State::Thinking;
                            continue;
                        }
                        self.state = State::Text;
                        continue;
                    }
                    if !OPEN.starts_with(trimmed) {
                        self.state = State::Text;
                        continue;
                    }
                    return out;
                }
                State::Thinking => {
                    if let Some(at) = self.buf.find(CLOSE) {
                        let reasoning = self.buf[..at].to_string();
                        self.buf = self.buf[at + CLOSE.len()..].to_string();
                        push_piece(&mut out, Piece::Reasoning(reasoning));
                        self.state = State::AfterThink;
                        continue;
                    }
                    let keep = partial_suffix(&self.buf, CLOSE);
                    let emit = self.buf[..self.buf.len() - keep].to_string();
                    self.buf = self.buf[self.buf.len() - keep..].to_string();
                    push_piece(&mut out, Piece::Reasoning(emit));
                    return out;
                }
                State::AfterThink => {
                    let trimmed = self.buf.trim_start();
                    if trimmed.is_empty() {
                        self.buf.clear();
                        return out;
                    }
                    self.buf = trimmed.to_string();
                    self.state = State::Text;
                }
                State::Text => {
                    let text = std::mem::take(&mut self.buf);
                    push_piece(&mut out, Piece::Text(text));
                    return out;
                }
            }
        }
    }

    /// Flushes what is left at the end of the stream.
    pub(crate) fn finish(&mut self) -> Vec<Piece> {
        let rest = std::mem::take(&mut self.buf);
        let piece = match self.state {
            State::Start | State::Text => Piece::Text(rest),
            State::Thinking => Piece::Reasoning(rest),
            State::AfterThink => return Vec::new(),
        };
        let mut out = Vec::new();
        push_piece(&mut out, piece);
        out
    }

    /// Splits complete content in one go.
    pub(crate) fn split(content: &str) -> (String, String) {
        let mut splitter = Self::default();
        let mut pieces = splitter.push(content);
        pieces.extend(splitter.finish());
        let (mut reasoning, mut text) = (String::new(), String::new());
        for piece in pieces {
            match piece {
                Piece::Reasoning(r) => reasoning.push_str(&r),
                Piece::Text(t) => text.push_str(&t),
            }
        }
        (reasoning, text)
    }
}

fn push_piece(out: &mut Vec<Piece>, piece: Piece) {
    let empty = match &piece {
        Piece::Reasoning(s) | Piece::Text(s) => s.is_empty(),
    };
    if !empty {
        out.push(piece);
    }
}

/// The length of the longest suffix of `text` that is a proper prefix of
/// `tag` (a tag possibly split across chunks).
fn partial_suffix(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| {
            text.len() >= n
                && text.is_char_boundary(text.len() - n)
                && tag.starts_with(&text[text.len() - n..])
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> Vec<Piece> {
        let mut splitter = ThinkTagSplitter::default();
        let mut out: Vec<Piece> = chunks.iter().flat_map(|c| splitter.push(c)).collect();
        out.extend(splitter.finish());
        // Merge adjacent pieces of the same kind for easy comparison.
        let mut merged: Vec<Piece> = Vec::new();
        for piece in out {
            match (merged.last_mut(), piece) {
                (Some(Piece::Reasoning(a)), Piece::Reasoning(b))
                | (Some(Piece::Text(a)), Piece::Text(b)) => a.push_str(&b),
                (_, piece) => merged.push(piece),
            }
        }
        merged
    }

    #[test]
    fn a_leading_think_block_is_split_across_any_chunking() {
        let full = "  <think>plan it</think>\n\nThe answer.";
        let expected = vec![
            Piece::Reasoning("plan it".into()),
            Piece::Text("The answer.".into()),
        ];
        assert_eq!(run(&[full]), expected);
        let chars: Vec<String> = full.chars().map(String::from).collect();
        let refs: Vec<&str> = chars.iter().map(String::as_str).collect();
        assert_eq!(run(&refs), expected);
        assert_eq!(
            run(&["<thi", "nk>a</th", "ink>b"]),
            vec![Piece::Reasoning("a".into()), Piece::Text("b".into())]
        );
    }

    #[test]
    fn content_without_a_leading_tag_is_untouched() {
        assert_eq!(
            run(&["Hello <think>x</think>"]),
            vec![Piece::Text("Hello <think>x</think>".into())]
        );
        assert_eq!(run(&["<thing>"]), vec![Piece::Text("<thing>".into())]);
        assert_eq!(run(&["<th"]), vec![Piece::Text("<th".into())]);
        assert_eq!(run(&[""]), Vec::<Piece>::new());
    }

    #[test]
    fn an_unterminated_think_block_is_reasoning() {
        assert_eq!(
            run(&["<think>still going</thi"]),
            vec![Piece::Reasoning("still going</thi".into())]
        );
        assert_eq!(
            run(&["<think>x</think>   "]),
            vec![Piece::Reasoning("x".into())]
        );
    }

    #[test]
    fn split_returns_reasoning_and_text() {
        assert_eq!(
            ThinkTagSplitter::split("<think>r</think>t"),
            ("r".into(), "t".into())
        );
        assert_eq!(
            ThinkTagSplitter::split("plain"),
            (String::new(), "plain".into())
        );
    }

    #[test]
    fn partial_suffix_handles_multibyte_text() {
        assert_eq!(partial_suffix("abc</th", CLOSE), 4);
        assert_eq!(partial_suffix("é", CLOSE), 0);
        assert_eq!(partial_suffix("x<", CLOSE), 1);
    }
}
