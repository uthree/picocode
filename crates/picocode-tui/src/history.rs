//! Shell-style input history: `↑`/`↓` in the input box recall previously
//! submitted messages.

#[derive(Default)]
pub struct InputHistory {
    entries: Vec<String>,
    /// Position while browsing (None = not browsing).
    index: Option<usize>,
    /// The unsubmitted input stashed when browsing starts, restored when
    /// stepping forward past the newest entry.
    stash: String,
}

impl InputHistory {
    /// Record a submitted message (consecutive duplicates collapse).
    pub fn push(&mut self, text: &str) {
        if self.entries.last().map(String::as_str) != Some(text) {
            self.entries.push(text.to_string());
        }
        self.index = None;
    }

    pub fn browsing(&self) -> bool {
        self.index.is_some()
    }

    /// An edit ends browsing; the recalled text stays in the input.
    pub fn stop(&mut self) {
        self.index = None;
    }

    /// Step to the previous (older) entry; `current` is stashed when
    /// browsing starts. None = already at the oldest entry (or no history).
    pub fn prev(&mut self, current: &str) -> Option<String> {
        let i = match self.index {
            None if !self.entries.is_empty() => {
                self.stash = current.to_string();
                self.entries.len() - 1
            }
            Some(i) if i > 0 => i - 1,
            _ => return None,
        };
        self.index = Some(i);
        Some(self.entries[i].clone())
    }

    /// Step to the next (newer) entry; past the newest one the stashed
    /// input is restored. None = not browsing.
    pub fn next(&mut self) -> Option<String> {
        match self.index {
            Some(i) if i + 1 < self.entries.len() => {
                self.index = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
            Some(_) => {
                self.index = None;
                Some(std::mem::take(&mut self.stash))
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_history_recalls_like_a_shell() {
        let mut h = InputHistory::default();
        // Nothing to recall yet.
        assert_eq!(h.prev("typing"), None);
        assert_eq!(h.next(), None);

        h.push("first");
        h.push("second");
        h.push("second"); // consecutive duplicate collapses
        h.push("third");

        // ↑ stashes the in-progress input and walks back…
        assert_eq!(h.prev("typing").as_deref(), Some("third"));
        assert_eq!(h.prev("ignored").as_deref(), Some("second"));
        assert_eq!(h.prev("ignored").as_deref(), Some("first"));
        // …and stops at the oldest entry.
        assert_eq!(h.prev("ignored"), None);
        assert!(h.browsing());

        // ↓ walks forward and restores the stashed input past the newest.
        assert_eq!(h.next().as_deref(), Some("second"));
        assert_eq!(h.next().as_deref(), Some("third"));
        assert_eq!(h.next().as_deref(), Some("typing"));
        assert!(!h.browsing());
        assert_eq!(h.next(), None);

        // An edit ends browsing; the next ↑ starts from the newest again.
        assert_eq!(h.prev("").as_deref(), Some("third"));
        h.stop();
        assert!(!h.browsing());
        assert_eq!(h.prev("edited").as_deref(), Some("third"));
        assert_eq!(h.next().as_deref(), Some("edited"));
    }
}
