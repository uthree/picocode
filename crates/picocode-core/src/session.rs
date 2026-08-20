//! Session persistence: each conversation is saved as a JSON file under the
//! user data directory, keyed by project, and can be restored with `/resume`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rig::completion::Message;
use serde::{Deserialize, Serialize};

use crate::transcript::{Entry, EntryKind};

const VERSION: u32 = 1;

/// One saved session: the model-side history plus the rendered transcript.
#[derive(Serialize, Deserialize)]
pub struct SessionFile {
    version: u32,
    pub cwd: String,
    /// Model label the session was last saved with (informational).
    pub model: String,
    pub history: Vec<Message>,
    pub entries: Vec<Entry>,
}

impl SessionFile {
    pub fn new(cwd: String, model: String, history: Vec<Message>, entries: Vec<Entry>) -> Self {
        Self {
            version: VERSION,
            cwd,
            model,
            history,
            entries,
        }
    }
}

/// One row in the `/resume` listing.
pub struct SessionSummary {
    pub id: String,
    pub modified: SystemTime,
    pub model: String,
    pub messages: usize,
    /// First user prompt, squeezed onto one line.
    pub snippet: String,
}

/// Where this project's sessions live:
/// `$XDG_DATA_HOME`/picocode/sessions/<project-slug> (default `~/.local/share`).
pub fn sessions_dir(root: &Path) -> Option<PathBuf> {
    sessions_dir_keyed(&root.display().to_string())
}

/// Sessions directory for a config: a remote workspace is keyed by its
/// host+path (so remote sessions don't collide with a same-named local
/// project and always live on the local machine).
pub fn sessions_dir_for(cfg: &crate::config::Config) -> Option<PathBuf> {
    sessions_dir_keyed(&match &cfg.remote {
        Some(spec) => spec.store_key(),
        None => cfg.root.display().to_string(),
    })
}

fn sessions_dir_keyed(text: &str) -> Option<PathBuf> {
    let base = data_dir()?.join("picocode/sessions");
    let dir = base.join(keyed_slug(text));
    migrate_legacy(&base.join(legacy_slug(text)), &dir);
    Some(dir)
}

/// Base directory for picocode's data: `$XDG_DATA_HOME`, defaulting to
/// `~/.local/share` (with `%USERPROFILE%` as the home on Windows).
/// Public so front ends can keep their own persisted files next to the
/// sessions and state (e.g. the GUI's settings).
pub fn data_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(x) => Some(PathBuf::from(x)),
        None => Some(crate::config::home_dir()?.join(".local/share")),
    }
}

/// Filesystem-safe store key for a project path or remote target: a
/// readable part where every non-alphanumeric character becomes `-`, plus a
/// short digest of the original text.
///
/// The readable part alone is not injective — `~/work/a-b` and `~/work/a/b`
/// both flatten to `-work-a-b`, and two unrelated projects then shared one
/// session store and one saved-model file, each seeing the other's
/// conversations. The digest separates them while keeping the directory
/// name recognisable.
pub(crate) fn keyed_slug(text: &str) -> String {
    let digest = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(text));
    format!("{}-{}", legacy_slug(text), &digest[..8])
}

/// The pre-digest name, kept only to find a store written by an older
/// version. It is exactly the prefix of [`keyed_slug`]'s output.
pub(crate) fn legacy_slug(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

/// Move a store saved under the pre-digest name into place, once. A missing
/// old entry is the normal case; a rename that cannot happen leaves the old
/// entry untouched and simply starts the new store empty.
pub(crate) fn migrate_legacy(old: &Path, new: &Path) {
    if !new.exists()
        && old.exists()
        && let Some(parent) = new.parent()
    {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::rename(old, new);
    }
}

/// A new unique session id. The counter disambiguates ids created within the
/// same second by the same process (e.g. rapid `/clear`).
pub fn new_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{secs}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Snapshot the conversation to disk: take the worker's history and write
/// it together with the rendered transcript. Both front ends spawn this
/// after each completed turn; empty conversations are not written, and a
/// write failure surfaces as an `Error` event.
pub async fn autosave(
    dir: PathBuf,
    id: String,
    cwd: String,
    model: String,
    entries: Vec<Entry>,
    cmd_tx: tokio::sync::mpsc::Sender<crate::event::WorkerCmd>,
    event_tx: tokio::sync::mpsc::Sender<crate::event::AgentEvent>,
) {
    let (htx, hrx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(crate::event::WorkerCmd::TakeHistory(htx))
        .await
        .is_err()
    {
        return;
    }
    let Ok(history) = hrx.await else {
        return;
    };
    if history.is_empty() {
        return;
    }
    let file = SessionFile::new(cwd, model, history, entries);
    if let Err(e) = save(&dir, &id, &file) {
        let _ = event_tx
            .send(crate::event::AgentEvent::Error(format!(
                "Failed to save session: {e:#}"
            )))
            .await;
    }
}

/// Write the session atomically (tmp file + rename).
pub fn save(dir: &Path, id: &str, session: &SessionFile) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let json = serde_json::to_vec(session)?;
    // Unique temp name: concurrent autosaves of the same session (e.g. a
    // turn completing and an auto-compaction right after it) must not race
    // on one temp file — the loser's rename would fail with ENOENT.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{id}.json.tmp{}-{seq}", std::process::id()));
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&tmp, json).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to rename to {}", path.display()))?;
    Ok(())
}

pub fn load(dir: &Path, id: &str) -> anyhow::Result<SessionFile> {
    let path = dir.join(format!("{id}.json"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no session file at {}", path.display()))?;
    let session: SessionFile = serde_json::from_str(&text)
        .with_context(|| format!("invalid session file {}", path.display()))?;
    if session.version != VERSION {
        anyhow::bail!(
            "session {} has unsupported version {} (expected {VERSION})",
            path.display(),
            session.version
        );
    }
    Ok(session)
}

/// Delete a saved session. A file that is already gone is not an error:
/// the row the user clicked may have been removed by another window.
pub fn delete(dir: &Path, id: &str) -> anyhow::Result<()> {
    let path = dir.join(format!("{id}.json"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("failed to delete {}", path.display())),
    }
}

/// All readable sessions in the directory, most recently saved first.
/// Unreadable or incompatible files are skipped.
pub fn list(dir: &Path) -> Vec<SessionSummary> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<SessionSummary> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<SessionFile>(&text) else {
            continue;
        };
        if session.version != VERSION {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        let snippet = session
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::User)
            .map(|e| one_line(&e.text, 48))
            .unwrap_or_default();
        out.push(SessionSummary {
            id: id.to_string(),
            modified,
            model: session.model,
            messages: session.history.len(),
            snippet,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Human-readable age of a timestamp ("just now", "5m ago", …).
pub fn age(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn one_line(s: &str, max_chars: usize) -> String {
    let mut out: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect();
    if out.chars().count() == max_chars && s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(model: &str, prompt: &str) -> SessionFile {
        SessionFile::new(
            "/tmp/proj".into(),
            model.into(),
            vec![Message::user(prompt), Message::assistant("hi!")],
            vec![
                Entry {
                    kind: EntryKind::Notice,
                    text: "picocode".into(),
                    lang: None,
                    attachments: Vec::new(),
                },
                Entry {
                    kind: EntryKind::User,
                    text: prompt.into(),
                    lang: None,
                    attachments: vec!["shot.png".into()],
                },
                Entry {
                    kind: EntryKind::Assistant,
                    text: "hi!".into(),
                    lang: None,
                    attachments: Vec::new(),
                },
            ],
        )
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample("ollama/qwen3:4b", "hello there");
        save(dir.path(), "s1", &session).unwrap();
        let loaded = load(dir.path(), "s1").unwrap();
        assert_eq!(loaded.model, "ollama/qwen3:4b");
        // Compare the wire representation: rig's flattened `additional_params`
        // deserializes None as Some({}), so struct equality is too strict.
        assert_eq!(
            serde_json::to_value(&loaded.history).unwrap(),
            serde_json::to_value(&session.history).unwrap()
        );
        assert_eq!(loaded.entries.len(), 3);
        assert!(load(dir.path(), "missing").is_err());
    }

    #[test]
    fn list_sorts_newest_first_and_skips_garbage() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "old", &sample("m1", "first session prompt")).unwrap();
        // Ensure a strictly later mtime for the second file.
        std::thread::sleep(std::time::Duration::from_millis(20));
        save(dir.path(), "new", &sample("m2", "second session prompt")).unwrap();
        std::fs::write(dir.path().join("junk.json"), "not json").unwrap();

        let sessions = list(dir.path());
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "new");
        assert_eq!(sessions[0].model, "m2");
        assert_eq!(sessions[0].messages, 2);
        assert_eq!(sessions[0].snippet, "second session prompt");
        assert_eq!(sessions[1].id, "old");
    }

    #[test]
    fn delete_removes_the_file_and_forgives_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "s1", &sample("m", "p")).unwrap();
        delete(dir.path(), "s1").unwrap();
        assert!(list(dir.path()).is_empty());
        assert!(load(dir.path(), "s1").is_err());
        // Deleting again (another window got there first) is a no-op.
        delete(dir.path(), "s1").unwrap();
    }

    #[test]
    fn snippets_are_squeezed_and_truncated() {
        assert_eq!(one_line("a\n b\tc", 48), "a b c");
        let long = "x".repeat(60);
        let snip = one_line(&long, 48);
        assert_eq!(snip.chars().count(), 49);
        assert!(snip.ends_with('…'));
    }

    #[test]
    fn ids_are_unique_and_slug_is_filesystem_safe() {
        assert_ne!(new_id(), new_id());
        let s = keyed_slug("/a/b c/d");
        assert!(s.starts_with("-a-b-c-d-"), "{s}");
        assert!(s.chars().all(|c| c.is_alphanumeric() || c == '-'), "{s}");
    }

    /// Two paths that flatten to the same readable name must not share a
    /// store: they used to, and each project saw the other's sessions.
    #[test]
    fn slugs_of_different_paths_never_collide() {
        assert_eq!(legacy_slug("/work/a-b"), legacy_slug("/work/a/b"));
        assert_ne!(keyed_slug("/work/a-b"), keyed_slug("/work/a/b"));
        // Same path, same key — the digest is of the text, not of a clock.
        assert_eq!(keyed_slug("/work/a-b"), keyed_slug("/work/a-b"));
    }

    #[test]
    fn a_store_under_the_old_name_is_moved_over() {
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join(legacy_slug("/work/proj"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("s.json"), "{}").unwrap();

        let new = base.path().join(keyed_slug("/work/proj"));
        migrate_legacy(&old, &new);
        assert!(new.join("s.json").exists());
        assert!(!old.exists());

        // An existing new store is never overwritten by a stale old one.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("other.json"), "{}").unwrap();
        migrate_legacy(&old, &new);
        assert!(!new.join("other.json").exists());
        assert!(old.exists());
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = sample("m", "p");
        session.version = 999;
        save(dir.path(), "v", &session).unwrap();
        assert!(load(dir.path(), "v").is_err());
        assert!(list(dir.path()).is_empty());
    }
}
