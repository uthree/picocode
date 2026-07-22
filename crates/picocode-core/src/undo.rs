//! Per-turn undo journal for agent file edits.
//!
//! Before `edit_file` writes a file, it records the file's previous state
//! (bytes, or "did not exist") into the current turn's frame — first record
//! per path wins, so the frame always holds the state from before the turn.
//! `/undo` restores the newest frame and pops it; calling it again walks
//! further back. Only `edit_file` writes are journaled: bash side effects
//! (and `!` commands) are out of scope.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Frames kept before the oldest is dropped.
const MAX_FRAMES: usize = 20;

/// A file's state before the turn touched it (`None` = did not exist).
type Original = Option<Vec<u8>>;

#[derive(Default)]
struct Frame {
    /// Insertion-ordered; one entry per path (the first record wins).
    files: Vec<(PathBuf, Original)>,
}

/// Shared, thread-safe stack of per-turn frames. Cloned into the edit tool
/// and the worker.
#[derive(Clone, Default)]
pub struct UndoJournal(Arc<Mutex<Vec<Frame>>>);

/// What `undo` did to one file.
#[derive(Debug, PartialEq, Eq)]
pub enum Restored {
    /// Previous contents written back.
    Reverted(PathBuf),
    /// The turn created it; it was deleted again.
    Removed(PathBuf),
    /// Restoring failed (the path stays as it is).
    Failed(PathBuf, String),
}

impl UndoJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a fresh frame for the coming turn. A trailing empty frame (a
    /// turn without edits) is reused so the stack only ever holds frames
    /// with something to undo.
    pub fn begin_turn(&self) {
        let mut stack = self.0.lock().unwrap();
        if stack.last().is_none_or(|f| !f.files.is_empty()) {
            stack.push(Frame::default());
        }
        if stack.len() > MAX_FRAMES {
            stack.remove(0);
        }
    }

    /// Record `path`'s current state into the open frame, before an edit
    /// overwrites it. Later records for the same path in the same frame are
    /// ignored (the pre-turn state is what undo restores). Without an open
    /// frame (no turn yet) this is a no-op.
    pub fn record(&self, path: &Path) {
        let mut stack = self.0.lock().unwrap();
        let Some(frame) = stack.last_mut() else {
            return;
        };
        if frame.files.iter().any(|(p, _)| p == path) {
            return;
        }
        let original = match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            // Unreadable (permissions?): the edit itself will fail too;
            // nothing sensible to record.
            Err(_) => return,
        };
        frame.files.push((path.to_path_buf(), original));
    }

    /// Restore the newest non-empty frame and pop it. Returns what happened
    /// per file, oldest-recorded first; empty when there is nothing to undo.
    pub fn undo(&self) -> Vec<Restored> {
        let frame = {
            let mut stack = self.0.lock().unwrap();
            loop {
                match stack.pop() {
                    Some(f) if f.files.is_empty() => continue,
                    other => break other,
                }
            }
        };
        let Some(frame) = frame else {
            return Vec::new();
        };
        frame
            .files
            .into_iter()
            .map(|(path, original)| match original {
                Some(bytes) => match std::fs::write(&path, bytes) {
                    Ok(()) => Restored::Reverted(path),
                    Err(e) => Restored::Failed(path, e.to_string()),
                },
                None => match std::fs::remove_file(&path) {
                    Ok(()) => Restored::Removed(path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Restored::Removed(path),
                    Err(e) => Restored::Failed(path, e.to_string()),
                },
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverts_edits_and_removes_creations() {
        let dir = tempfile::tempdir().unwrap();
        let edited = dir.path().join("a.txt");
        let created = dir.path().join("new.txt");
        std::fs::write(&edited, "before").unwrap();

        let j = UndoJournal::new();
        j.begin_turn();
        j.record(&edited);
        std::fs::write(&edited, "after").unwrap();
        j.record(&created);
        std::fs::write(&created, "fresh").unwrap();

        let restored = j.undo();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "before");
        assert!(!created.exists());
        // Nothing further to undo.
        assert!(j.undo().is_empty());
    }

    #[test]
    fn first_record_per_path_wins_and_frames_stack() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v1").unwrap();

        let j = UndoJournal::new();
        j.begin_turn();
        j.record(&f);
        std::fs::write(&f, "v2").unwrap();
        j.record(&f); // second edit in the same turn: still restores v1
        std::fs::write(&f, "v3").unwrap();

        j.begin_turn();
        j.record(&f);
        std::fs::write(&f, "v4").unwrap();

        j.undo();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v3");
        j.undo();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
    }

    #[test]
    fn empty_turns_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "old").unwrap();

        let j = UndoJournal::new();
        j.begin_turn();
        j.record(&f);
        std::fs::write(&f, "new").unwrap();
        j.begin_turn(); // turn without edits
        j.begin_turn(); // another

        let restored = j.undo();
        assert_eq!(restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "old");
    }

    #[test]
    fn record_without_frame_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "x").unwrap();
        let j = UndoJournal::new();
        j.record(&f);
        assert!(j.undo().is_empty());
    }
}
