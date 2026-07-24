//! IC-1 compiler integration checkpoint (issue #9).
//!
//! These tests exercise the committed multi-prompt fixture exclusively
//! through the public `cybersin` CLI. Later runtime checkpoints can consume
//! the same committed `dist/` without inventing their own compiler output.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").unwrap()
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ic1-research-team")
}

fn copy_project_without_dist(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "dist" {
            continue;
        }
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn collect_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(base: &Path, current: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(current).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                visit(base, &entry.path(), files);
            } else {
                let relative = entry
                    .path()
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, fs::read(entry.path()).unwrap());
            }
        }
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn git(repo: &Path, args: &[&str]) {
    let status = ProcessCommand::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn ic1_sample_builds_frozen_through_the_full_compiler() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("ic1-research-team");
    copy_project_without_dist(&fixture(), &project);

    cybersin()
        .arg("check")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("2 source(s) ok"));
    cybersin()
        .arg("build")
        .arg(&project)
        .args(["--profile", "release", "--frozen"])
        .assert()
        .success();

    let dist = project.join("dist");
    for expected in [
        "manifest.json",
        "routing.json",
        "cache.json",
        "prompts/researcher.json",
        "prompts/researcher/generic.json",
        "prompts/researcher/openai.json",
        "prompts/synthesizer.json",
        "prompts/synthesizer/generic.json",
        "prompts/synthesizer/openai.json",
        "budget/researcher.json",
        "budget/synthesizer.json",
    ] {
        assert!(dist.join(expected).is_file(), "missing dist/{expected}");
    }
    assert!(dist.join("evals").is_dir());
    assert!(project.join("evals/researcher.eval.yaml").is_file());

    let routing: serde_json::Value =
        serde_json::from_slice(&fs::read(dist.join("routing.json")).unwrap()).unwrap();
    assert_eq!(routing["prompts"].as_object().unwrap().len(), 2);
    assert_eq!(
        routing["prompts"]["researcher"]["decisions"][0]["kind"],
        "cache"
    );
    assert_eq!(
        routing["prompts"]["researcher"]["decisions"][1]["kind"],
        "cascade"
    );
    assert_eq!(
        routing["prompts"]["researcher"]["decisions"][2]["kind"],
        "fallbacks"
    );

    let researcher: serde_json::Value =
        serde_json::from_slice(&fs::read(dist.join("prompts/researcher.json")).unwrap()).unwrap();
    assert!(researcher["sections"][0]["body"]
        .as_str()
        .unwrap()
        .contains("Cite evidence"));
    let openai: serde_json::Value =
        serde_json::from_slice(&fs::read(dist.join("prompts/researcher/openai.json")).unwrap())
            .unwrap();
    assert_eq!(openai["target"], "openai");
    assert_eq!(openai["tools"][0]["type"], "function");
}

#[test]
fn committed_dist_matches_a_frozen_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("ic1-research-team");
    copy_project_without_dist(&fixture(), &project);
    cybersin()
        .arg("build")
        .arg(&project)
        .args(["--profile", "release", "--frozen"])
        .assert()
        .success();

    let mut committed = collect_files(&fixture().join("dist"));
    let mut rebuilt = collect_files(&project.join("dist"));
    let mut committed_manifest: serde_json::Value =
        serde_json::from_slice(committed.get("manifest.json").unwrap()).unwrap();
    let mut rebuilt_manifest: serde_json::Value =
        serde_json::from_slice(rebuilt.get("manifest.json").unwrap()).unwrap();
    committed_manifest["git_sha"] = serde_json::Value::Null;
    rebuilt_manifest["git_sha"] = serde_json::Value::Null;
    committed.remove("manifest.json");
    // Issue #21 replaces this tracked empty-directory marker with compiled
    // eval suites. The issue #9 builder intentionally emits an empty evals/
    // directory until that compiler stage lands.
    committed.remove("evals/.gitkeep");
    rebuilt.remove("manifest.json");

    assert_eq!(rebuilt, committed);
    assert_eq!(rebuilt_manifest, committed_manifest);
}

#[test]
fn diff_reports_a_meaningful_ic1_source_change() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let project = repo.join("fixtures/ic1-research-team");
    copy_project_without_dist(&fixture(), &project);
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "IC-1 baseline"]);

    let prompt = project.join("prompts/researcher.prompt.yaml");
    let changed = fs::read_to_string(&prompt).unwrap().replace(
        "Investigate {{ topic }}",
        "Investigate {{ topic }} rigorously",
    );
    fs::write(prompt, changed).unwrap();

    cybersin()
        .arg("diff")
        .arg("HEAD")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("M  prompts/researcher.json"))
        .stdout(predicate::str::contains("changed"))
        .stdout(predicate::str::contains("0 added, 0 removed"));
}
