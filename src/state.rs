//! Tiny persisted per-project state: the last-used model, restored on the
//! next start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The model selection to restore on the next start. Both the `[[models]]`
/// entry name and the resolved provider/model are stored, so the selection
/// survives the entry being renamed or removed.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct LastModel {
    /// `[[models]]` entry name the selection came from, if any.
    #[serde(default)]
    pub entry: Option<String>,
    /// Provider name ("ollama", "openai", "anthropic").
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

/// Where this project's state lives:
/// `$XDG_DATA_HOME/picocode/state/<project-slug>.json` (default
/// `~/.local/share/…`, with `%USERPROFILE%` as the home on Windows).
pub fn state_path(root: &Path) -> Option<PathBuf> {
    Some(
        crate::session::data_dir()?
            .join("picocode/state")
            .join(format!("{}.json", crate::session::slug(root))),
    )
}

/// Load the saved state; any unreadable or invalid file counts as none.
pub fn load(path: &Path) -> Option<LastModel> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn save(path: &Path, state: &LastModel) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrips_and_tolerates_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/proj.json");

        assert_eq!(load(&path), None);

        let state = LastModel {
            entry: Some("local".into()),
            provider: "ollama".into(),
            model: "qwen3:4b".into(),
            base_url: None,
        };
        save(&path, &state).unwrap();
        assert_eq!(load(&path), Some(state));

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), None);
    }
}
