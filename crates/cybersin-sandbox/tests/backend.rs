use std::fs;

use cybersin_sandbox::{
    DockerBackend, ExecRequest, GvisorBackend, LimitKind, ResourceLimits, SandboxBackend,
    SandboxScope, Termination,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn fake_runtime(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("docker");
    fs::write(&runtime, script).unwrap();
    let mut permissions = fs::metadata(&runtime).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&runtime, permissions).unwrap();
    (temp, runtime)
}

#[cfg(unix)]
#[test]
fn docker_backend_executes_in_a_default_deny_limited_workspace() {
    let (_runtime_dir, runtime) = fake_runtime(
        r#"#!/bin/sh
case "$*" in
  *"--network none"*"--cpus 1"*"--memory 64m"*"--pids-limit 32"*"--read-only"*"--rm"*) ;;
  *) echo "missing containment flags: $*" >&2; exit 90 ;;
esac
printf 'sandbox output'
"#,
    );
    let workspace = tempfile::tempdir().unwrap();
    let backend = DockerBackend::with_binary(runtime);

    let outcome = backend
        .exec(ExecRequest {
            image: "example/tool:locked".into(),
            command: vec!["sh".into(), "-c".into(), "echo hello".into()],
            workspace: workspace.path().to_path_buf(),
            scope: SandboxScope::Call,
            limits: ResourceLimits {
                cpus: 1.0,
                memory_mb: 64,
                pids: 32,
                wall_clock: std::time::Duration::from_secs(5),
            },
        })
        .unwrap();

    assert!(outcome.succeeded());
    assert_eq!(outcome.stdout, "sandbox output");
}

#[cfg(unix)]
#[test]
fn wall_clock_limit_is_an_inspectable_outcome() {
    let (_runtime_dir, runtime) = fake_runtime(
        r#"#!/bin/sh
sleep 5
"#,
    );
    let workspace = tempfile::tempdir().unwrap();
    let backend = DockerBackend::with_binary(runtime);

    let outcome = backend
        .exec(ExecRequest {
            image: "example/tool:locked".into(),
            command: vec!["long-task".into()],
            workspace: workspace.path().to_path_buf(),
            scope: SandboxScope::Call,
            limits: ResourceLimits {
                wall_clock: std::time::Duration::from_millis(50),
                ..ResourceLimits::default()
            },
        })
        .unwrap();

    assert_eq!(
        outcome.termination,
        Termination::KilledByLimit(LimitKind::WallClock)
    );
    assert!(!outcome.succeeded());
}

#[cfg(unix)]
#[test]
fn gvisor_backend_selects_the_runsc_runtime() {
    let (_runtime_dir, runtime) = fake_runtime(
        r#"#!/bin/sh
case "$*" in
  *"--runtime=runsc"*) printf 'isolated by runsc' ;;
  *) echo "runsc runtime was not selected" >&2; exit 91 ;;
esac
"#,
    );
    let workspace = tempfile::tempdir().unwrap();
    let backend = GvisorBackend::with_binary(runtime);

    let outcome = backend
        .exec(ExecRequest {
            image: "example/tool:locked".into(),
            command: vec!["true".into()],
            workspace: workspace.path().to_path_buf(),
            scope: SandboxScope::Call,
            limits: ResourceLimits::default(),
        })
        .unwrap();

    assert!(outcome.succeeded());
    assert_eq!(outcome.stdout, "isolated by runsc");
}

#[cfg(unix)]
#[test]
fn container_resource_kills_are_inspectable_outcomes() {
    let (_runtime_dir, runtime) = fake_runtime("#!/bin/sh\nexit 137\n");
    let workspace = tempfile::tempdir().unwrap();
    let outcome = DockerBackend::with_binary(runtime)
        .exec(ExecRequest {
            image: "example/tool:locked".into(),
            command: vec!["hostile-payload".into()],
            workspace: workspace.path().to_path_buf(),
            scope: SandboxScope::Call,
            limits: ResourceLimits::default(),
        })
        .unwrap();

    assert_eq!(
        outcome.termination,
        Termination::KilledByLimit(LimitKind::ContainerResource)
    );
}
