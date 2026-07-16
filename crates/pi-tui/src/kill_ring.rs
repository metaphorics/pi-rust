//! Emacs-style kill ring — port of packages/tui/src/kill-ring.ts.

/// Ring buffer for kill/yank operations.
///
/// Consecutive kills can accumulate into a single entry. Supports yank
/// (paste most recent) and yank-pop (cycle through older entries).
#[derive(Debug, Clone, Default)]
pub struct KillRing {
    ring: Vec<String>,
}

impl KillRing {
    pub fn new() -> Self {
        Self { ring: Vec::new() }
    }

    /// Add text to the kill ring.
    ///
    /// - `prepend`: when accumulating, prepend (backward delete) or append (forward)
    /// - `accumulate`: merge with the most recent entry instead of creating a new one
    pub fn push(&mut self, text: &str, prepend: bool, accumulate: bool) {
        if text.is_empty() {
            return;
        }

        if accumulate && !self.ring.is_empty() {
            let last = self.ring.pop().unwrap_or_default();
            let merged = if prepend {
                format!("{text}{last}")
            } else {
                format!("{last}{text}")
            };
            self.ring.push(merged);
        } else {
            self.ring.push(text.to_owned());
        }
    }

    /// Most recent entry without modifying the ring.
    pub fn peek(&self) -> Option<&str> {
        self.ring.last().map(String::as_str)
    }

    /// Move last entry to front (yank-pop cycling).
    pub fn rotate(&mut self) {
        if self.ring.len() > 1
            && let Some(last) = self.ring.pop()
        {
            self.ring.insert(0, last);
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_peek_rotate() {
        let mut k = KillRing::new();
        k.push("a", false, false);
        k.push("b", false, false);
        assert_eq!(k.peek(), Some("b"));
        k.rotate();
        assert_eq!(k.peek(), Some("a"));
        k.rotate();
        assert_eq!(k.peek(), Some("b"));
    }
    #[test]
    fn accumulate_append_and_prepend() {
        let mut k = KillRing::new();
        k.push("hello", false, false);
        k.push(" world", false, true); // append while accumulating
        assert_eq!(k.peek(), Some("hello world"));

        let mut k2 = KillRing::new();
        k2.push("world", false, false);
        k2.push("hello ", true, true); // prepend while accumulating
        assert_eq!(k2.peek(), Some("hello world"));
    }

    #[test]
    fn empty_text_ignored() {
        let mut k = KillRing::new();
        k.push("", false, false);
        assert_eq!(k.len(), 0);
    }
}
