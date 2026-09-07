use std::path::Path;
use std::process::Command;

use picocode_core::config::{Args, Config, Mode};
use picocode_core::{git, session};

fn run_git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn worktrees_start_at_head_and_keep_edits_and_sessions_separate() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("source repo");
    std::fs::create_dir(&root).unwrap();
    run_git(&root, &["init", "-b", "main"]);
    std::fs::write(root.join("file.txt"), "committed").unwrap();
    run_git(&root, &["add", "."]);
    run_git(
        &root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "initial",
        ],
    );
    std::fs::write(root.join("file.txt"), "source edits").unwrap();
    let store = temp.path().join("session store");
    let first = git::create_worktree(&root, &store.join("worktrees"), "first").unwrap();
    let second = git::create_worktree(&first, &store.join("worktrees"), "second").unwrap();
    assert_eq!(git::branch(&first).as_deref(), Some("picocode/first"));
    assert_eq!(git::branch(&root).as_deref(), Some("main"));
    assert_eq!(
        std::fs::read_to_string(first.join("file.txt")).unwrap(),
        "committed"
    );
    std::fs::write(first.join("file.txt"), "first edits").unwrap();
    assert_eq!(
        std::fs::read_to_string(second.join("file.txt")).unwrap(),
        "committed"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).unwrap(),
        "source edits"
    );
    let saved =
        session::SessionFile::new(first.display().to_string(), "test".into(), vec![], vec![]);
    session::save(&store, "first", &saved).unwrap();
    assert_eq!(
        session::load(&store, "first").unwrap().cwd,
        first.display().to_string()
    );
    session::delete(&store, "first").unwrap();
    assert_eq!(
        std::fs::read_to_string(first.join("file.txt")).unwrap(),
        "first edits"
    );
    assert!(git::create_worktree(&root, &store.join("worktrees"), "first").is_err());
    assert_eq!(
        std::fs::read_to_string(first.join("file.txt")).unwrap(),
        "first edits"
    );
}

#[test]
fn invalid_worktree_requests_do_not_touch_existing_files() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("worktrees");
    assert!(git::create_worktree(temp.path(), &store, "../outside").is_err());
    assert!(!store.exists());
    assert!(git::create_worktree(temp.path(), &store, "not-a-repo").is_err());
    run_git(temp.path(), &["init"]);
    assert!(git::create_worktree(temp.path(), &store, "no-commit").is_err());
    assert!(!store.join("no-commit").exists());
}

#[test]
fn sessions_copy_settings_without_sharing_live_changes_or_changing_cwd() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("picocode.toml"), "bash_timeout = 42\n").unwrap();
    std::fs::write(
        temp.path().join("AGENTS.md"),
        "Workspace-specific instructions",
    )
    .unwrap();
    let cwd = std::env::current_dir().unwrap();
    let original = Config::from_args_in(Args::for_workspace(None), temp.path()).unwrap();
    assert_eq!(std::env::current_dir().unwrap(), cwd);
    assert_eq!(original.root, temp.path());
    assert!(
        original
            .instructions
            .iter()
            .any(|(_, text)| text.contains("Workspace-specific"))
    );
    let second = original.for_session();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    std::fs::write(worktree.join("AGENTS.md"), "Worktree instructions").unwrap();
    let restored = original.in_local_workspace(&worktree).unwrap();
    assert_eq!(restored.root, worktree);
    assert!(
        restored
            .instructions
            .iter()
            .any(|(_, text)| text == "Worktree instructions")
    );
    assert_eq!(std::env::current_dir().unwrap(), cwd);
    assert!(
        original
            .in_local_workspace(&temp.path().join("missing"))
            .is_err()
    );
    let worker = second.clone();
    second.mode.set(Mode::Edit);
    second.bash_timeout.set(123);
    second.read_max_lines.set(124);
    second.read_max_line_bytes.set(125);
    second.auto_compact.set(26);
    second.max_tokens.set(127);
    second.context_window.set(128);
    second.context_window_max.set(129);
    second.search.set_max_results(19);
    second.approval.allow_tool("session-specific-tool");
    assert_eq!(worker.bash_timeout.get(), 123);
    assert_eq!(worker.mode.get(), Mode::Edit);
    assert_eq!(original.bash_timeout.get(), 42);
    assert_eq!(original.mode.get(), Mode::ReadOnly);
    for (a, b) in [
        (&original.read_max_lines, &second.read_max_lines),
        (&original.read_max_line_bytes, &second.read_max_line_bytes),
        (&original.auto_compact, &second.auto_compact),
        (&original.max_tokens, &second.max_tokens),
        (&original.context_window, &second.context_window),
        (&original.context_window_max, &second.context_window_max),
    ] {
        assert_ne!(a.get(), b.get());
    }
    assert_ne!(
        original.search.snapshot().max_results,
        second.search.snapshot().max_results
    );
    assert!(
        !original
            .approval
            .snapshot()
            .allow_tools
            .contains(&"session-specific-tool".into())
    );
}
