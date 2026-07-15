//! Generic undo stack — port of packages/tui/src/undo-stack.ts.

/// Clone-on-push undo stack.
///
/// Stores clones of state snapshots. Popped snapshots are returned
/// directly (already detached).
#[derive(Debug, Clone)]
pub struct UndoStack<S> {
    stack: Vec<S>,
}

impl<S> Default for UndoStack<S> {
    fn default() -> Self {
        Self { stack: Vec::new() }
    }
}

impl<S> UndoStack<S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remove all snapshots.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }
}

impl<S: Clone> UndoStack<S> {
    /// Push a clone of the given state onto the stack.
    pub fn push(&mut self, state: &S) {
        self.stack.push(state.clone());
    }

    /// Pop and return the most recent snapshot, or `None` if empty.
    pub fn pop(&mut self) -> Option<S> {
        self.stack.pop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_clear() {
        let mut u = UndoStack::new();
        let s1 = String::from("one");
        let s2 = String::from("two");
        u.push(&s1);
        u.push(&s2);
        assert_eq!(u.len(), 2);
        assert_eq!(u.pop().as_deref(), Some("two"));
        assert_eq!(u.pop().as_deref(), Some("one"));
        assert!(u.pop().is_none());
        u.push(&s1);
        u.clear();
        assert!(u.is_empty());
    }
}
