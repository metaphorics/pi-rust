//! Append-friendly shared text for streaming `partial` snapshots.
//!
//! Each delta event needs a historically-correct `partial` without copying the
//! entire accumulated character buffer (which is O(n²) over a long completion).
//! Text is stored as a persistent cons chain of `Arc` nodes: append is O(1),
//! historical roots remain valid, and serialize walks the chain once into a
//! preallocated `String`.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug)]
struct Node {
    prev: Option<Arc<Node>>,
    chunk: Arc<str>,
    total_len: usize,
}

/// Immutable, append-only text with structural sharing across snapshots.
#[derive(Clone, Debug, Default)]
pub struct SharedText {
    tip: Option<Arc<Node>>,
}

impl SharedText {
    pub fn new() -> Self {
        Self::default()
    }

    // This infallible generic constructor intentionally differs from `FromStr`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
        if value.is_empty() {
            Self::default()
        } else {
            Self {
                tip: Some(Arc::new(Node {
                    prev: None,
                    chunk: Arc::from(value),
                    total_len: value.len(),
                })),
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.tip.as_ref().map_or(0, |node| node.total_len)
    }

    /// O(1) append: returns a new tip that shares the previous chain.
    pub fn append(&self, delta: &str) -> Self {
        if delta.is_empty() {
            return self.clone();
        }
        let prev_len = self.len();
        Self {
            tip: Some(Arc::new(Node {
                prev: self.tip.clone(),
                chunk: Arc::from(delta),
                total_len: prev_len + delta.len(),
            })),
        }
    }

    pub fn as_string(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for SharedText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut stack = Vec::new();
        let mut cursor = self.tip.as_ref();
        while let Some(node) = cursor {
            stack.push(node);
            cursor = node.prev.as_ref();
        }
        for node in stack.into_iter().rev() {
            f.write_str(&node.chunk)?;
        }
        Ok(())
    }
}

impl From<&str> for SharedText {
    fn from(value: &str) -> Self {
        Self::from_str(value)
    }
}

impl From<String> for SharedText {
    fn from(value: String) -> Self {
        Self::from_str(value)
    }
}

impl PartialEq for SharedText {
    fn eq(&self, other: &Self) -> bool {
        if self.len() != other.len() {
            return false;
        }
        match (&self.tip, &other.tip) {
            (Some(a), Some(b)) if Arc::ptr_eq(a, b) => true,
            _ => self.to_string() == other.to_string(),
        }
    }
}

impl Eq for SharedText {}

impl Serialize for SharedText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut out = String::with_capacity(self.len());
        let mut stack = Vec::new();
        let mut cursor = self.tip.as_ref();
        while let Some(node) = cursor {
            stack.push(node);
            cursor = node.prev.as_ref();
        }
        for node in stack.into_iter().rev() {
            out.push_str(&node.chunk);
        }
        serializer.serialize_str(&out)
    }
}

impl<'de> Deserialize<'de> for SharedText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_str(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_is_o1_and_preserves_historical_roots() {
        let root = SharedText::from_str("hello");
        let mid = root.append(" ");
        let end = mid.append("world");
        assert_eq!(root.to_string(), "hello");
        assert_eq!(mid.to_string(), "hello ");
        assert_eq!(end.to_string(), "hello world");
        // Historical tip still points at the original first chunk node.
        assert!(Arc::ptr_eq(
            root.tip.as_ref().unwrap(),
            end.tip
                .as_ref()
                .unwrap()
                .prev
                .as_ref()
                .unwrap()
                .prev
                .as_ref()
                .unwrap()
        ));
    }

    #[test]
    fn long_append_chain_stays_linear_in_len() {
        let mut text = SharedText::new();
        for i in 0..200 {
            text = text.append(&format!("{i},"));
        }
        assert_eq!(text.len(), text.to_string().len());
        assert!(text.to_string().starts_with("0,1,2,"));
    }
}
