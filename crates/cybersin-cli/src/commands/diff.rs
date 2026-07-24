//! `cybersin diff <ref>` (spec §7, §11): builds the current project and
//! the same project checked out at another git ref, then reports which
//! `dist/` artifacts changed — the PR-review workflow spec §7 promises
//! ("A PR therefore shows exactly which compressed rewrite or price
//! update changed").
//!
//! This repo doesn't commit `dist/` output (nothing under `dist/` is
//! tracked, and `.gitignore` doesn't need to say so), so there's no
//! committed artifact to diff directly — a fresh build of both refs is
//! the only source of truth. `git worktree` checks the other ref out
//! into a throwaway directory so the caller's actual working tree (and
//! its `cybersin.lock`) are never touched.
//!
//! Both sides build `--frozen` unconditionally: a diff is a read-only
//! comparison and must never trigger a network call as a side effect of
//! running it (spec §7). The profile defaults to `dev` so `diff` works
//! even before anything is pinned in `cybersin.lock`; pass `--profile
//! release` to see compressed-rewrite diffs too, once compression *is*
//! pinned (a frozen release build still refuses to compress anything
//! that isn't).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::commands::build::{run_into, BuildProfile};
use crate::git;

pub fn run(
    project: &Path,
    reference: &str,
    profile: BuildProfile,
) -> Result<Option<String>, String> {
    let repo_root = git::show_toplevel(project)?;
    let project_abs = fs::canonicalize(project)
        .map_err(|e| format!("error: failed to resolve {}: {e}", project.display()))?;
    let relative_project = project_abs.strip_prefix(&repo_root).map_err(|_| {
        format!(
            "error: {} is not inside git repo {}",
            project.display(),
            repo_root.display()
        )
    })?;

    let workdir =
        tempfile::tempdir().map_err(|e| format!("error: failed to create a temp dir: {e}"))?;
    let current_dist = workdir.path().join("current");
    let ref_dist = workdir.path().join("ref");
    let ref_worktree = workdir.path().join("worktree");

    run_into(project, &current_dist, profile, true, None)
        .map_err(|e| format!("error: failed to build current sources: {e}"))?;

    git::worktree_add(&repo_root, &ref_worktree, reference)?;
    let ref_build = run_into(
        &ref_worktree.join(relative_project),
        &ref_dist,
        profile,
        true,
        None,
    )
    .map_err(|e| format!("error: failed to build {reference}: {e}"));
    git::worktree_remove(&repo_root, &ref_worktree);
    ref_build?;

    let before = collect_files(&ref_dist)?;
    let after = collect_files(&current_dist)?;

    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(before.keys());
    paths.extend(after.keys());

    let mut added = 0usize;
    let mut removed = 0usize;
    let mut changed = 0usize;
    for path in paths {
        match (before.get(path), after.get(path)) {
            (None, Some(_)) => {
                added += 1;
                println!("A  {path}");
            }
            (Some(_), None) => {
                removed += 1;
                println!("D  {path}");
            }
            (Some(old), Some(new)) if old != new => {
                changed += 1;
                println!("M  {path}");
                print!(
                    "{}",
                    line_diff(&String::from_utf8_lossy(old), &String::from_utf8_lossy(new))
                );
            }
            _ => {}
        }
    }

    Ok(Some(format!(
        "cybersin diff {reference}: {added} added, {removed} removed, {changed} changed"
    )))
}

fn collect_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut files = BTreeMap::new();
    if root.is_dir() {
        walk(root, root, &mut files)?;
    }
    Ok(files)
}

fn walk(base: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("error: failed to read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("error: failed to read {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, files)?;
        } else {
            let bytes = fs::read(&path)
                .map_err(|e| format!("error: failed to read {}: {e}", path.display()))?;
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(relative, bytes);
        }
    }
    Ok(())
}

/// Minimal LCS-based line diff. No diff crate is a workspace
/// dependency, and `dist/` artifacts are small pretty-printed JSON, so
/// a plain O(n·m) DP table is fast enough without adding one.
fn line_diff(old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let n = old_lines.len();
    let m = new_lines.len();
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old_lines[i] == new_lines[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push_str("  - ");
            out.push_str(old_lines[i]);
            out.push('\n');
            i += 1;
        } else {
            out.push_str("  + ");
            out.push_str(new_lines[j]);
            out.push('\n');
            j += 1;
        }
    }
    while i < n {
        out.push_str("  - ");
        out.push_str(old_lines[i]);
        out.push('\n');
        i += 1;
    }
    while j < m {
        out.push_str("  + ");
        out.push_str(new_lines[j]);
        out.push('\n');
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo_with_project(repo: &Path) {
        fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        crate::commands::init::run(&repo.join("project")).expect("init");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn diff_reports_a_changed_prompt_between_two_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo_with_project(&repo);

        let before_sha = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        let prompt_path = repo.join("project/prompts/hello.prompt.yaml");
        let updated = fs::read_to_string(&prompt_path)
            .unwrap()
            .replace("warmly", "with maximum enthusiasm");
        fs::write(&prompt_path, updated).unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "reword greeting"]);

        let summary = run(&repo.join("project"), &before_sha, BuildProfile::Dev)
            .expect("diff")
            .expect("summary message");
        assert!(summary.contains("changed"));
        assert!(!summary.contains("0 added, 0 removed, 0 changed"));
    }

    #[test]
    fn diff_against_an_identical_ref_reports_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo_with_project(&repo);

        let summary = run(&repo.join("project"), "HEAD", BuildProfile::Dev)
            .expect("diff")
            .expect("summary message");
        assert!(
            summary.contains("0 added, 0 removed, 0 changed"),
            "{summary}"
        );
    }
}
