//! Integration tests for the `cybersin` CLI: `check`, `init`, `fmt`
//! (spec §11), exercised by shelling out to the built binary via
//! `assert_cmd`, matching this issue's acceptance criteria end-to-end.

use std::collections::BTreeSet;
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
            "next",
        ])
        .assert()
        .success();
    cybersin()
        .args([
            "--db",
            db.to_str().unwrap(),
            "sessions",
            "resume",
            "durable-1",
            "--config-hash",
            "next",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("resumed durable-1"));
}
