//! Small `git` shell-outs shared by `build`'s `manifest.json` `git_sha`
//! and `diff`'s ref checkout. Deliberately not a crate dependency
//! (`git2` et al.) — spawning the `git` binary covers the handful of
//! read-only operations this CLI needs (CLAUDE.md: no speculative
//! generality).

use std::path::{Path, PathBuf};
use std::process::Command;

/// `git rev-parse HEAD` in `dir`, or `"unknown"` if `dir` isn't inside a
/// git repo (or `git` isn't on `PATH`) — a documented placeholder
/// rather than a hard build failure. `manifest.json`'s byte-identical-
/// rebuild guarantee doesn't depend on this being the real SHA, only on
/// it being the same value across two consecutive builds of the same
/// tree, which "unknown" satisfies just as well as a real one.
pub fn sha(dir: &Path) -> String {
    output(dir, &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
}

/// The git repo root containing `dir`, for `cybersin diff`'s ref
/// checkout — it needs the whole repo, not just the project subtree.
pub fn show_toplevel(dir: &Path) -> Result<PathBuf, String> {
    output(dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "error: {} is not inside a git repository (required for `cybersin diff`)",
                dir.display()
            )
        })
}

/// Check out `reference` into a new, detached worktree at `dest`, so
/// `cybersin diff` can build it without disturbing the caller's actual
/// working tree.
pub fn worktree_add(repo_root: &Path, dest: &Path, reference: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(dest)
        .arg(reference)
        .current_dir(repo_root)
        .status()
        .map_err(|e| format!("error: failed to run git worktree add: {e}"))?;
    if !status.success() {
        return Err(format!(
            "error: git worktree add failed for ref {reference:?} (exit {status})"
        ));
    }
    Ok(())
}

/// Remove a worktree created by [`worktree_add`]. Best-effort cleanup —
/// called after the diff has already succeeded or failed, so a removal
/// failure shouldn't mask that outcome.
pub fn worktree_remove(repo_root: &Path, dest: &Path) {
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(dest)
        .current_dir(repo_root)
        .status();
}

fn output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
