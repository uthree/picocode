//! Per-turn undo journal for agent file edits.
//!
//! Before `edit_file` writes a file, it records the file's previous state
//! (bytes, or "did not exist") into the current turn's frame — first record
//! per path wins, so the frame always holds the state from before the turn.
//! `/undo` restores the newest frame and pops it; calling it again walks
//! further back. Only `edit_file` writes are journaled: bash side effects
//! (and `!` commands) are out of scope.
//!
//! All I/O goes through the workspace backend, so `/undo` works on a remote
//! workspace instead of quietly operating on same-named local paths. And a
//! file the user has edited by hand since the turn is left alone rather than
//! silently rolled back over: the read stamps `edit_file` keeps say whether
//! a path still holds what picocode last wrote there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::backend::Workspace;
use crate::tools::ReadStamps;

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
#[derive(Clone)]
pub struct UndoJournal {
    frames: Arc<Mutex<Vec<Frame>>>,
    ws: Workspace,
    /// The mtimes `read_file`/`edit_file` recorded, used to tell a file
    /// picocode last wrote from one the user has changed since.
    stamps: ReadStamps,
}

/// What `undo` did to one file.
#[derive(Debug, PartialEq, Eq)]
pub enum Restored {
    /// Previous contents written back.
    Reverted(PathBuf),
    /// The turn created it; it was deleted again.
    Removed(PathBuf),
    /// Changed outside picocode since the turn, so it was left as it is —
    /// undoing would have thrown that work away.
    Skipped(PathBuf),
    /// Restoring failed (the path stays as it is).
    Failed(PathBuf, String),
}

impl UndoJournal {
    pub fn new(ws: Workspace, stamps: ReadStamps) -> Self {
        Self {
            frames: Arc::new(Mutex::new(Vec::new())),
            ws,
            stamps,
        }
    }

    /// Open a fresh frame for the coming turn. A trailing empty frame (a
    /// turn without edits) is reused so the stack only ever holds frames
    /// with something to undo.
    pub fn begin_turn(&self) {
        let mut stack = self.frames.lock().unwrap();
        if stack.last().is_none_or(|f| !f.files.is_empty()) {
            stack.push(Frame::default());
        }
        if stack.len() > MAX_FRAMES {
            stack.remove(0);
        }
    }

    /// Whether the open frame already holds `path` (the first record per
    /// path is the one undo restores). Also false when no turn is open.
    fn already_recorded(&self, path: &Path) -> bool {
        let stack = self.frames.lock().unwrap();
        match stack.last() {
            Some(frame) => frame.files.iter().any(|(p, _)| p == path),
            // No frame yet: nothing to add to, so treat it as done.
            None => true,
        }
    }

    /// Record `path`'s current state into the open frame, before an edit
    /// overwrites it. Later records for the same path in the same frame are
    /// ignored (the pre-turn state is what undo restores). Without an open
    /// frame (no turn yet) this is a no-op.
    pub async fn record(&self, path: &Path) {
        if self.already_recorded(path) {
            return;
        }
        let original = match self.ws.backend.read(path).await {
            Ok(bytes) => Some(bytes),
            // A failed read is either "not there yet" (the edit creates it)
            // or "cannot be read at all". The remote backend reports both
            // the same way, so ask separately.
            Err(_) if !self.ws.backend.is_file(path).await => None,
            // Unreadable (permissions?): the edit itself will fail too;
            // nothing sensible to record.
            Err(_) => return,
        };
        // The read above was an await point, so re-check under the lock.
        let mut stack = self.frames.lock().unwrap();
        let Some(frame) = stack.last_mut() else {
            return;
        };
        if frame.files.iter().any(|(p, _)| p == path) {
            return;
        }
        frame.files.push((path.to_path_buf(), original));
    }

    /// Restore the newest non-empty frame and pop it. Returns what happened
    /// per file, oldest-recorded first; empty when there is nothing to undo.
    pub async fn undo(&self) -> Vec<Restored> {
        let frame = {
            let mut stack = self.frames.lock().unwrap();
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
        let backend = &self.ws.backend;
        let mut out = Vec::with_capacity(frame.files.len());
        for (path, original) in frame.files {
            // The file moved since picocode last wrote it, so somebody else
            // owns its current contents. Rolling back would discard them.
            if self.stamps.is_stale(backend, &path).await {
                out.push(Restored::Skipped(path));
                continue;
            }
            out.push(match original {
                Some(bytes) => match backend.write(&path, &bytes).await {
                    Ok(()) => {
                        self.stamps.record(backend, &path).await;
                        Restored::Reverted(path)
                    }
                    Err(e) => Restored::Failed(path, e.to_string()),
                },
                None => match backend.remove_file(&path).await {
                    Ok(()) => Restored::Removed(path),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Restored::Removed(path),
                    Err(e) => Restored::Failed(path, e.to_string()),
                },
            });
        }
        out
    }
}

/// Tests of *other* tools build an `EditFile` and never call `/undo`, so an
/// empty workspace is enough for them: `record` without an open frame is a
/// no-op regardless of the root.
#[cfg(test)]
impl Default for UndoJournal {
    fn default() -> Self {
        Self::new(Workspace::local(PathBuf::new()), ReadStamps::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(root: &Path) -> UndoJournal {
        UndoJournal::new(Workspace::local(root.to_path_buf()), ReadStamps::default())
    }

    #[tokio::test]
    async fn reverts_edits_and_removes_creations() {
        let dir = tempfile::tempdir().unwrap();
        let edited = dir.path().join("a.txt");
        let created = dir.path().join("new.txt");
        std::fs::write(&edited, "before").unwrap();

        let j = journal(dir.path());
        j.begin_turn();
        j.record(&edited).await;
        std::fs::write(&edited, "after").unwrap();
        j.record(&created).await;
        std::fs::write(&created, "fresh").unwrap();

        let restored = j.undo().await;
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "before");
        assert!(!created.exists());
        // Nothing further to undo.
        assert!(j.undo().await.is_empty());
    }

    #[tokio::test]
    async fn first_record_per_path_wins_and_frames_stack() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "v1").unwrap();

        let j = journal(dir.path());
        j.begin_turn();
        j.record(&f).await;
        std::fs::write(&f, "v2").unwrap();
        j.record(&f).await; // second edit in the same turn: still restores v1
        std::fs::write(&f, "v3").unwrap();

        j.begin_turn();
        j.record(&f).await;
        std::fs::write(&f, "v4").unwrap();

        j.undo().await;
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v3");
        j.undo().await;
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
    }

    #[tokio::test]
    async fn empty_turns_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "old").unwrap();

        let j = journal(dir.path());
        j.begin_turn();
        j.record(&f).await;
        std::fs::write(&f, "new").unwrap();
        j.begin_turn(); // turn without edits
        j.begin_turn(); // another

        let restored = j.undo().await;
        assert_eq!(restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "old");
    }

    #[tokio::test]
    async fn record_without_frame_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "x").unwrap();
        let j = journal(dir.path());
        j.record(&f).await;
        assert!(j.undo().await.is_empty());
    }

    /// The user edits a file after the turn that changed it. `/undo` must
    /// leave their work alone rather than restore over it.
    #[tokio::test]
    async fn a_file_edited_by_hand_since_the_turn_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let agent_edited = dir.path().join("a.txt");
        let user_edited = dir.path().join("b.txt");
        std::fs::write(&agent_edited, "before").unwrap();
        std::fs::write(&user_edited, "before").unwrap();

        let stamps = ReadStamps::default();
        let ws = Workspace::local(dir.path().to_path_buf());
        let j = UndoJournal::new(ws.clone(), stamps.clone());
        j.begin_turn();
        for path in [&agent_edited, &user_edited] {
            j.record(path).await;
            std::fs::write(path, "agent").unwrap();
            // What edit_file does after every successful write.
            stamps.record(&ws.backend, path).await;
        }

        // Only b.txt is touched afterwards. mtime has second granularity on
        // some filesystems, so set it explicitly rather than racing it.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        std::fs::write(&user_edited, "mine").unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&user_edited)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();

        let restored = j.undo().await;
        assert_eq!(
            restored,
            vec![
                Restored::Reverted(agent_edited.clone()),
                Restored::Skipped(user_edited.clone()),
            ]
        );
        assert_eq!(std::fs::read_to_string(&agent_edited).unwrap(), "before");
        assert_eq!(std::fs::read_to_string(&user_edited).unwrap(), "mine");
    }
}
