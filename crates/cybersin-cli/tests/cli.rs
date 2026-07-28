//! Integration tests for the `cybersin` CLI: `check`, `init`, `fmt`
//! (spec §11), exercised by shelling out to the built binary via
//! `assert_cmd`, matching this issue's acceptance criteria end-to-end.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").unwrap()
}

fn write_hello_prompt(project: &std::path::Path) {
    fs::write(
        project.join("fragments/tone.md"),
        "You are a friendly, concise assistant.\n",
    )
    .unwrap();
    fs::write(
        project.join("prompts/hello.prompt.yaml"),
        r#"name: hello
quality: medium
inputs:
  name: string
sections:
  - id: role
    priority: 100
    body: !include ../fragments/tone.md
  - id: instructions
    priority: 90
    body: |
      Greet {{ name }} warmly and briefly.
"#,
    )
    .unwrap();
}

fn show_session(db: &Path, session_id: &str) -> serde_json::Value {
    let output = cybersin()
        .args(["--db", db.to_str().unwrap(), "sessions", "show", session_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn migration_events(session: &serde_json::Value) -> Vec<&serde_json::Value> {
    session["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "session.migrated")
        .collect()
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn relative_paths(root: &Path) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            paths.insert(rel);
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    paths
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn collect_file_bytes(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                files.insert(rel, fs::read(path).unwrap());
            }
        }
    }
    files
}

#[test]
fn explicit_help_spellings_print_help_without_entering_tui() {
    for spelling in ["-help", "-h", "--help"] {
        cybersin()
            .arg(spelling)
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "Cybersin prompt compiler + agent runtime CLI",
            ))
            .stdout(predicate::str::contains("Usage: cybersin"));
    }
}

#[test]
fn bare_non_tty_invocation_fails_clearly_instead_of_hanging() {
    cybersin()
        .assert()
        .failure()
        .stderr(predicate::str::contains("requires an interactive terminal"))
        .stderr(predicate::str::contains("cybersin -help"));
}

#[test]
fn init_scaffolds_only_the_basic_project_spine() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");

    cybersin()
        .arg("init")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("scaffolded"))
        .stdout(predicate::str::contains("created:"))
        .stdout(predicate::str::contains("skipped: none"));

    for expected in [
        "cybersin.yaml",
        "cybersin.lock",
        "cybersin.local.example.yaml",
        ".gitignore",
        "prompts",
        "fragments",
        "evals",
        "agents",
        "tools",
    ] {
        assert!(project.join(expected).exists(), "missing {expected}");
    }

    for unexpected in [
        "prompts/hello.prompt.yaml",
        "fragments/tone.md",
        "evals/hello.eval.yaml",
        "agents/hello.agent.yaml",
        "loop.py",
        "inputs",
        "dist",
    ] {
        assert!(
            !project.join(unexpected).exists(),
            "unexpected scaffolded path {unexpected}"
        );
    }
}

#[test]
fn init_starter_template_creates_a_buildable_recorded_eval_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("starter");

    cybersin()
        .arg("init")
        .arg(&project)
        .arg("--template")
        .arg("starter")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "scaffolded cybersin starter project",
        ))
        .stdout(predicate::str::contains(
            "prompts/cybersin-starter.prompt.yaml",
        ))
        .stdout(predicate::str::contains(
            "inputs/cybersin-starter.input.json",
        ));

    for expected in [
        "cybersin.yaml",
        "cybersin.lock",
        "fragments/cybersin-starter-instructions.md",
        "prompts/cybersin-starter.prompt.yaml",
        "evals/cybersin-starter.eval.yaml",
        "inputs",
        "inputs/cybersin-starter.input.json",
    ] {
        assert!(project.join(expected).exists(), "missing {expected}");
    }
    assert!(
        !project.join("agents/cybersin-starter.agent.yaml").exists(),
        "starter template should rely on the built-in starter harness, not a user-authored harness"
    );

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();
    assert!(project.join("dist/prompts/cybersin-starter.json").exists());

    cybersin()
        .arg("eval")
        .arg("run")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "cybersin-starter.eval.yaml::starter_brief",
        ))
        .stdout(predicate::str::contains("PASS"));
}

#[test]
fn init_starter_template_honors_skip_force_and_dry_run_reporting() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("starter");
    fs::create_dir_all(project.join("prompts")).unwrap();
    fs::write(
        project.join("prompts/cybersin-starter.prompt.yaml"),
        "name: existing\n",
    )
    .unwrap();

    cybersin()
        .arg("init")
        .arg(&project)
        .arg("--template")
        .arg("starter")
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped:"))
        .stdout(predicate::str::contains(
            "prompts/cybersin-starter.prompt.yaml",
        ));
    assert_eq!(
        fs::read_to_string(project.join("prompts/cybersin-starter.prompt.yaml")).unwrap(),
        "name: existing\n"
    );

    let dry_run_project = tmp.path().join("dry-run-starter");
    cybersin()
        .arg("init")
        .arg(&dry_run_project)
        .arg("--template")
        .arg("starter")
        .arg("--dry-run")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "would scaffold cybersin starter project",
        ))
        .stdout(predicate::str::contains(
            "inputs/cybersin-starter.input.json",
        ));
    assert!(!dry_run_project.exists());

    cybersin()
        .arg("init")
        .arg(&project)
        .arg("--template")
        .arg("starter")
        .arg("--force")
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped: none"));
    assert_ne!(
        fs::read_to_string(project.join("prompts/cybersin-starter.prompt.yaml")).unwrap(),
        "name: existing\n"
    );
}

#[test]
fn init_with_setup_differs_from_plain_init_only_by_setup_phase() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tmp.path().join("plain");
    let with_setup = tmp.path().join("with-setup");
    fs::create_dir_all(&plain).unwrap();
    fs::create_dir_all(&with_setup).unwrap();
    fs::write(plain.join(".env"), "OPENROUTER_API_KEY=test-key\n").unwrap();
    fs::write(with_setup.join(".env"), "OPENROUTER_API_KEY=test-key\n").unwrap();

    cybersin().arg("init").arg(&plain).assert().success();
    cybersin()
        .arg("init")
        .arg(&with_setup)
        .arg("--setup")
        .env_remove("OPENROUTER_API_KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains("scaffolded"))
        .stdout(predicate::str::contains("Updated"))
        .stdout(predicate::str::contains("Cybersin doctor"));

    let plain_paths = relative_paths(&plain);
    let mut setup_paths = relative_paths(&with_setup);
    assert!(setup_paths.remove("cybersin.local.yaml"));
    assert_eq!(plain_paths, setup_paths);

    let local = fs::read_to_string(with_setup.join("cybersin.local.yaml")).unwrap();
    assert!(local.contains("api_key: ${OPENROUTER_API_KEY}"));
    assert!(local.contains("provider: openrouter"));
    assert!(!plain.join("cybersin.local.yaml").exists());
}

#[test]
fn init_with_setup_honors_init_overwrite_rules_for_existing_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join(".env"), "OPENROUTER_API_KEY=test-key\n").unwrap();
    fs::write(project.join("cybersin.yaml"), "name: existing\n").unwrap();

    cybersin()
        .arg("init")
        .arg(&project)
        .arg("--setup")
        .env_remove("OPENROUTER_API_KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped:"))
        .stdout(predicate::str::contains("cybersin.yaml"));
    assert_eq!(
        fs::read_to_string(project.join("cybersin.yaml")).unwrap(),
        "name: existing\n"
    );
    assert!(project.join("cybersin.local.yaml").exists());

    cybersin()
        .arg("init")
        .arg(&project)
        .arg("--force")
        .arg("--setup")
        .env_remove("OPENROUTER_API_KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped: none"));
    assert_ne!(
        fs::read_to_string(project.join("cybersin.yaml")).unwrap(),
        "name: existing\n"
    );
}

#[test]
fn doctor_reports_ready_setup_without_requiring_dist() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    fs::write(project.join(".env"), "OPENROUTER_API_KEY=test-key\n").unwrap();
    fs::write(
        project.join("cybersin.local.yaml"),
        "providers:\n  openrouter:\n    availability: available\ndefaults:\n  provider: openrouter\n  model: openai/gpt-4o-mini\npermissions:\n  routing:\n    allowed_providers: [openrouter]\n",
    )
    .unwrap();

    cybersin()
        .arg("--project")
        .arg(&project)
        .arg("doctor")
        .env_remove("OPENROUTER_API_KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains("Cybersin doctor"))
        .stdout(predicate::str::contains(
            "api key: ready via .env:OPENROUTER_API_KEY",
        ))
        .stdout(predicate::str::contains("[warn] dist/ missing"))
        .stdout(predicate::str::contains(
            "Run `cybersin build . --profile dev --frozen`",
        ));
}

#[test]
fn setup_creates_local_config_with_env_reference_and_runs_doctor() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    fs::write(project.join(".env"), "OPENROUTER_API_KEY=test-key\n").unwrap();

    cybersin()
        .arg("--project")
        .arg(&project)
        .arg("setup")
        .env_remove("OPENROUTER_API_KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains("Updated"))
        .stdout(predicate::str::contains("cybersin.local.yaml"))
        .stdout(predicate::str::contains("Cybersin doctor"))
        .stdout(predicate::str::contains(
            "api key: ready via cybersin.local.yaml -> .env:OPENROUTER_API_KEY",
        ));

    let local = fs::read_to_string(project.join("cybersin.local.yaml")).unwrap();
    assert!(local.contains("api_key: ${OPENROUTER_API_KEY}"));
    assert!(local.contains("provider: openrouter"));
    assert!(local.contains("allowed_providers:"));
}

#[test]
fn setup_rerun_is_idempotent_and_preserves_existing_local_config() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();
    fs::write(project.join(".env"), "CUSTOM_OPENROUTER_KEY=test-key\n").unwrap();
    fs::write(
        project.join("cybersin.local.yaml"),
        "providers:\n  openrouter:\n    api_key: ${CUSTOM_OPENROUTER_KEY}\ntools:\n  tavily:\n    availability: auto\n",
    )
    .unwrap();

    cybersin()
        .arg("--project")
        .arg(&project)
        .arg("setup")
        .env_remove("CUSTOM_OPENROUTER_KEY")
        .assert()
        .success();
    let after_first = fs::read_to_string(project.join("cybersin.local.yaml")).unwrap();

    cybersin()
        .arg("--project")
        .arg(&project)
        .arg("setup")
        .env_remove("CUSTOM_OPENROUTER_KEY")
        .assert()
        .success()
        .stdout(predicate::str::contains("already up to date"));
    let after_second = fs::read_to_string(project.join("cybersin.local.yaml")).unwrap();

    assert_eq!(after_first, after_second);
    assert!(after_second.contains("api_key: ${CUSTOM_OPENROUTER_KEY}"));
    assert!(after_second.contains("tavily:"));
}

#[test]
fn setup_reports_missing_key_guidance_from_doctor() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();

    cybersin()
        .arg("--project")
        .arg(&project)
        .arg("setup")
        .env_remove("OPENROUTER_API_KEY")
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "api key: OPENROUTER_API_KEY is referenced but not set",
        ))
        .stdout(predicate::str::contains(
            "Set OPENROUTER_API_KEY in .env or export it",
        ));
}

#[test]
fn setup_raw_secret_opt_in_is_explicit_but_rejected_by_current_config_model() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    cybersin().arg("init").arg(&project).assert().success();

    cybersin()
        .arg("--project")
        .arg(&project)
        .arg("setup")
        .arg("--raw-openrouter-api-key")
        .arg("sk-or-raw-secret")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--raw-openrouter-api-key is not supported by the current local config model",
        ));

    assert!(!project.join("cybersin.local.yaml").exists());
}

#[test]
fn init_skips_existing_files_unless_forced_and_supports_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("myagent");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("cybersin.yaml"), "name: existing\n").unwrap();

    cybersin()
        .arg("init")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped:"))
        .stdout(predicate::str::contains("cybersin.yaml"));
    assert_eq!(
        fs::read_to_string(project.join("cybersin.yaml")).unwrap(),
        "name: existing\n"
    );

    let dry_run_project = tmp.path().join("dry-run");
    cybersin()
        .arg("init")
        .arg("--dry-run")
        .arg(&dry_run_project)
        .assert()
        .success()
        .stdout(predicate::str::contains("would scaffold"))
        .stdout(predicate::str::contains("created:"));
    assert!(!dry_run_project.exists());

    cybersin()
        .arg("init")
        .arg("--force")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped: none"));
    assert_ne!(
        fs::read_to_string(project.join("cybersin.yaml")).unwrap(),
        "name: existing\n"
    );
}

#[test]
fn check_passes_on_a_hand_written_valid_source() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("fragments_method.md"),
        "Search broadly, then narrow.\n",
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("fragments")).unwrap();
    fs::write(
        tmp.path().join("fragments/research-method.md"),
        "Search broadly, then narrow.\n",
    )
    .unwrap();

    let source = r#"
name: researcher
quality: high
inputs:
  topic: string
  documents: list[document]
sections:
  - id: role
    priority: 100
    body: |
      You are a research analyst focused on {{ topic }}.
  - id: instructions
    priority: 90
    body: !include fragments/research-method.md
  - id: documents
    priority: 50
    body: "{{#each documents}}- {{this.title}}\n{{/each}}"
"#;
    let path = tmp.path().join("researcher.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin().arg("check").arg(&path).assert().success();
}

#[test]
fn check_fails_clearly_on_cyclic_include() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("fragments")).unwrap();
    fs::write(tmp.path().join("fragments/a.md"), "!include b.md\n").unwrap();
    fs::write(tmp.path().join("fragments/b.md"), "!include a.md\n").unwrap();

    let source = r#"
name: broken
quality: high
inputs:
  topic: string
sections:
  - id: role
    priority: 100
    body: !include fragments/a.md
"#;
    let path = tmp.path().join("broken.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin()
        .arg("check")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("include cycle"));
}

#[test]
fn check_fails_clearly_on_type_mismatch_and_unused_input() {
    let tmp = tempfile::tempdir().unwrap();
    let source = r#"
name: broken
quality: high
inputs:
  topic: string
  unused_one: string
sections:
  - id: role
    priority: 100
    body: "{{#each topic}}{{this}}{{/each}}"
"#;
    let path = tmp.path().join("broken.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin()
        .arg("check")
        .arg(&path)
        .assert()
        .failure()
        .stderr(predicate::str::contains("typecheck failed"))
        .stderr(predicate::str::contains("unused_one"));
}

#[test]
fn fmt_normalizes_a_prompt_source_file() {
    let tmp = tempfile::tempdir().unwrap();
    let source = r#"
sections:
  - priority: 100
    id: role
    body: hello {{ name }}
quality: high
inputs:
  b_input: string
  a_input: string
name: unsorted
"#;
    let path = tmp.path().join("unsorted.prompt.yaml");
    fs::write(&path, source).unwrap();

    cybersin()
        .arg("fmt")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("formatted"));

    let formatted = fs::read_to_string(&path).unwrap();
    let name_pos = formatted.find("name:").unwrap();
    let quality_pos = formatted.find("quality:").unwrap();
    let inputs_pos = formatted.find("inputs:").unwrap();
    let sections_pos = formatted.find("sections:").unwrap();
    assert!(name_pos < quality_pos && quality_pos < inputs_pos && inputs_pos < sections_pos);

    // idempotent: a second fmt --check run reports already-formatted.
    cybersin()
        .arg("fmt")
        .arg("--check")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("already formatted"));
}

#[test]
fn default_build_uses_dev_profile_and_succeeds_frozen_without_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--frozen")
        .assert()
        .success();
    assert!(project.join("dist/prompts/hello.json").exists());
}

#[test]
fn explicit_dev_build_excludes_compression_and_succeeds_frozen_without_pins() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();
    assert!(project.join("dist/prompts/hello.json").exists());
}

#[test]
fn explicit_release_build_runs_release_compression_path() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("release")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "compression provider failed: no compression provider configured",
        ));
}

#[test]
fn frozen_release_build_refuses_unpinned_compression() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("release")
        .arg("--frozen")
        .assert()
        .failure()
        .stderr(predicate::str::contains("would require a network call"));
}

#[test]
fn build_validates_prompt_sources_before_replacing_dist() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);

    cybersin().arg("build").arg(&project).assert().success();
    let manifest = fs::read_to_string(project.join("dist/manifest.json")).unwrap();

    fs::write(
        project.join("prompts/broken.prompt.yaml"),
        r#"
name: broken
quality: medium
inputs:
  unused: string
sections:
  - id: body
    priority: 100
    body: "Nothing references the input."
"#,
    )
    .unwrap();

    cybersin()
        .arg("build")
        .arg(&project)
        .assert()
        .failure()
        .stderr(predicate::str::contains("typecheck failed"));

    assert_eq!(
        fs::read_to_string(project.join("dist/manifest.json")).unwrap(),
        manifest
    );
}

#[test]
fn build_writes_the_full_dist_shape_and_renders_every_configured_target() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);

    // Add `openai` alongside the default `generic` target so both a
    // concrete model family and the portable target render (spec §6.5).
    let cybersin_yaml = fs::read_to_string(project.join("cybersin.yaml")).unwrap();
    fs::write(
        project.join("cybersin.yaml"),
        cybersin_yaml.replace("targets:\n  - generic", "targets:\n  - generic\n  - openai"),
    )
    .unwrap();

    cybersin()
        .arg("build")
        .arg(&project)
        .arg("--profile")
        .arg("dev")
        .arg("--frozen")
        .assert()
        .success();

    let dist = project.join("dist");
    assert!(dist.join("manifest.json").exists());
    assert!(dist.join("routing.json").exists());
    assert!(dist.join("cache.json").exists());
    assert!(dist.join("evals").is_dir());
    assert!(dist.join("budget/hello.json").exists());
    assert!(dist.join("prompts/hello.json").exists());
    assert!(dist.join("prompts/hello/generic.json").exists());
    assert!(dist.join("prompts/hello/openai.json").exists());

    let openai_rendered: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dist.join("prompts/hello/openai.json")).unwrap())
            .unwrap();
    assert_eq!(openai_rendered["target"], "openai");
    assert!(openai_rendered["messages"][0]["content"]
        .as_str()
        .unwrap()
        .contains("<section"));

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(dist.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["artifacts"]
        .as_object()
        .unwrap()
        .contains_key("prompts/hello/openai.json"));
}

#[test]
fn diff_reports_a_change_against_head_via_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);

    let project = repo.join("project");
    cybersin().arg("init").arg(&project).assert().success();
    write_hello_prompt(&project);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "initial"]);

    let prompt_path = project.join("prompts/hello.prompt.yaml");
    let updated = fs::read_to_string(&prompt_path)
        .unwrap()
        .replace("warmly", "with great enthusiasm");
    fs::write(&prompt_path, updated).unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "reword"]);

    cybersin()
        .arg("diff")
        .arg("HEAD~1")
        .arg(&project)
        .assert()
        .success()
        .stdout(predicate::str::contains("changed"))
        .stdout(predicate::str::contains("prompts/hello"));
}

#[test]
fn durable_session_cli_lists_shows_notifies_migrates_and_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.db");

    // No `cybersin.yaml` lives above this tempdir, so `--dist` must be
    // passed explicitly (issue #50): omitting it now errors rather than
    // silently falling back to the bundled stub fixture.
    let dist = cybersin_runtime::bundled_stub_dist_dir();
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--dist",
            dist.to_str().unwrap(),
            "run",
            "--stub",
            "--session-id",
            "durable-1",
        ])
        .assert()
        .success();
    cybersin()
        .args(["--db", db.to_str().unwrap(), "sessions", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("durable-1"));
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "show",
            "durable-1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"checkpoint\""));
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "notify",
            "durable-1",
            "{\"go\":true}",
        ])
        .assert()
        .success();
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "migrate",
            "durable-1",
            "--config-hash",
            "missing-hash",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing-hash"))
        .stderr(predicate::str::contains(
            "run a build/run that ingests it first",
        ));
    let after_failed_migrate = show_session(&db, "durable-1");
    assert_eq!(after_failed_migrate["config_hash"], "stub-manual-0001");
    assert!(migration_events(&after_failed_migrate).is_empty());
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "migrate",
            "durable-1",
            "--config-hash",
            "stub-manual-0001",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "migrated durable-1 to stub-manual-0001",
        ));
    let after_stored_migrate = show_session(&db, "durable-1");
    assert_eq!(after_stored_migrate["config_hash"], "stub-manual-0001");
    let migrations = migration_events(&after_stored_migrate);
    assert_eq!(migrations.len(), 1);
    assert_eq!(migrations[0]["payload"]["config_hash"], "stub-manual-0001");
    let materialized = tmp.path().join("materialized-dist");
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "materialize",
            "--session",
            "durable-1",
            "--out",
            materialized.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("materialized"));
    assert_eq!(
        fs::read(materialized.join("manifest.json")).unwrap(),
        fs::read(dist.join("manifest.json")).unwrap()
    );
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "resume",
            "durable-1",
            "--config-hash",
            "stub-manual-0001",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("resumed durable-1"));
}

#[test]
fn sessions_materialize_restores_full_ingested_bundle_from_unrelated_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.db");
    let project = tmp.path().join("project");
    let source_dist = project.join("dist");
    let expected_dist = tmp.path().join("relocated-source-dist");
    let unrelated = tmp.path().join("unrelated");
    let materialized_by_session = tmp.path().join("materialized-by-session");
    let materialized_by_hash = tmp.path().join("materialized-by-hash");
    let config_hash = "stub-manual-0001";

    copy_tree(&cybersin_runtime::bundled_stub_dist_dir(), &source_dist);
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "--dist",
            source_dist.to_str().unwrap(),
            "run",
            "--stub",
            "--session-id",
            "materialize-1",
        ])
        .current_dir(&project)
        .assert()
        .success();

    copy_tree(&source_dist, &expected_dist);
    fs::remove_dir_all(&source_dist).unwrap();
    fs::create_dir_all(&unrelated).unwrap();

    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "materialize",
            "--session",
            "materialize-1",
            "--out",
            materialized_by_session.to_str().unwrap(),
        ])
        .current_dir(&unrelated)
        .assert()
        .success()
        .stdout(predicate::str::contains(config_hash));
    assert_eq!(
        collect_file_bytes(&materialized_by_session),
        collect_file_bytes(&expected_dist)
    );

    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "materialize",
            "--config-hash",
            config_hash,
            "--out",
            materialized_by_hash.to_str().unwrap(),
        ])
        .current_dir(&unrelated)
        .assert()
        .success()
        .stdout(predicate::str::contains(config_hash));
    assert_eq!(
        collect_file_bytes(&materialized_by_hash),
        collect_file_bytes(&expected_dist)
    );
}

#[test]
fn sessions_materialize_missing_pinned_hash_reports_hash_without_creating_target() {
    use cybersin_runtime::Storage as _;

    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.db");
    let target = tmp.path().join("out");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        cybersin_runtime::SqliteStorage::connect(&format!("sqlite://{}?mode=rwc", db.display()))
            .await
            .unwrap()
            .create_session_pinned("missing-bundle", "agent", "absent-hash-92")
            .await
            .unwrap();
    });

    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "materialize",
            "--session",
            "missing-bundle",
            "--out",
            target.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("absent-hash-92"));
    assert!(!target.exists());
}

#[tokio::test]
async fn sessions_ls_surfaces_heartbeat_liveness_and_json() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.db");
    let daemon = cybersin_runtime::DaemonHandle::auto_start(&db)
        .await
        .unwrap();
    let storage = daemon.storage();
    storage
        .create_session("live-session", "agent-a")
        .await
        .unwrap();
    storage
        .write_session_heartbeat("live-session", now_unix_ms(), "pid=1 host=test")
        .await
        .unwrap();
    storage
        .create_session("stale-session", "agent-b")
        .await
        .unwrap();
    storage
        .write_session_heartbeat("stale-session", 1, "pid=2 host=test")
        .await
        .unwrap();
    storage
        .create_session("done-session", "agent-c")
        .await
        .unwrap();
    storage
        .write_session_heartbeat("done-session", 1, "pid=3 host=test")
        .await
        .unwrap();
    storage
        .set_session_status("done-session", "completed")
        .await
        .unwrap();
    drop(daemon);

    cybersin()
        .args(["--db", db.to_str().unwrap(), "sessions", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("live-session"))
        .stdout(predicate::str::contains("live"))
        .stdout(predicate::str::contains("pid=1 host=test"))
        .stdout(predicate::str::contains("stale-session"))
        .stdout(predicate::str::contains("stale"))
        .stdout(predicate::str::contains("done-session"))
        .stdout(predicate::str::contains("terminal"));

    let output = cybersin()
        .args(["--db", db.to_str().unwrap(), "sessions", "ls", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let row = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["session_id"] == id)
            .unwrap()
    };
    assert_eq!(row("live-session")["liveness"], "live");
    assert_eq!(row("live-session")["heartbeat_holder"], "pid=1 host=test");
    assert!(row("live-session")["last_heartbeat_unix_ms"].is_number());
    assert_eq!(row("stale-session")["liveness"], "stale");
    assert_eq!(row("done-session")["liveness"], "terminal");
}

#[test]
fn run_start_ingests_artifacts_idempotently_and_audits_outcome() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("state.db");
    let dist = cybersin_runtime::bundled_stub_dist_dir();
    let config_hash = "stub-manual-0001";

    for session_id in ["ingest-1", "ingest-2"] {
        cybersin()
            .args([
                "--db",
                db.to_str().unwrap(),
                "--dist",
                dist.to_str().unwrap(),
                "run",
                "--stub",
                "--session-id",
                session_id,
            ])
            .assert()
            .success();
    }

    let first = show_session(&db, "ingest-1");
    let second = show_session(&db, "ingest-2");
    assert_eq!(first["config_hash"], config_hash);
    assert_eq!(second["config_hash"], config_hash);

    let artifact_event = |session: &serde_json::Value| {
        session["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "artifact.bundle")
            .unwrap()
            .clone()
    };
    let first_artifact = artifact_event(&first);
    let second_artifact = artifact_event(&second);

    assert_eq!(first_artifact["payload"]["config_hash"], config_hash);
    assert_eq!(first_artifact["payload"]["outcome"], "stored");
    assert_eq!(second_artifact["payload"]["config_hash"], config_hash);
    assert_eq!(second_artifact["payload"]["outcome"], "reused");
    assert!(first_artifact["payload"]["file_count"].as_u64().unwrap() > 1);
}
