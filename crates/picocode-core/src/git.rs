//! Minimal git repository info for the status displays — reads `.git`
//! directly instead of pulling in a git library or shelling out.

use std::path::{Path, PathBuf};

/// The current branch of the repository containing `root`, if any:
/// the branch name, or a short commit hash when HEAD is detached.
/// Walks up from `root` like git does, and follows `.git` files
/// (worktrees / submodules).
pub fn branch(root: &Path) -> Option<String> {
    let mut dir = root;
    loop {
        let dot_git = dir.join(".git");
        if dot_git.exists() {
            return read_head(&dot_git);
        }
        dir = dir.parent()?;
    }
}

fn read_head(dot_git: &Path) -> Option<String> {
    // A `.git` *file* points elsewhere: "gitdir: <path>".
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(dot_git).ok()?;
        let target = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target
        } else {
            dot_git.parent()?.join(target)
        }
    } else {
        dot_git.to_path_buf()
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        ),
        // Detached HEAD: a bare commit hash.
        None => Some(head.chars().take(8).collect()),
    }
}

/// `root` for display: the home-directory prefix shortened to `~`.
pub fn display_dir(root: &Path) -> String {
    let display = root.display().to_string();
    match crate::config::home_dir() {
        Some(home) => match root.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => display,
        },
        None => display,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branches_detached_heads_and_gitfile_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(branch(root), None);

        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        // Found from a subdirectory too, with the full branch name.
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(branch(&sub), Some("feature/x".to_string()));

        std::fs::write(root.join(".git/HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(branch(root), Some("01234567".to_string()));

        // Worktree-style `.git` file.
        let wt = root.join("wt");
        std::fs::create_dir_all(wt.join("real-git")).unwrap();
        std::fs::write(wt.join("real-git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        std::fs::write(wt.join(".git"), "gitdir: real-git\n").unwrap();
        assert_eq!(branch(&wt), Some("dev".to_string()));
    }
}
