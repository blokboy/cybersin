use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn cybersin() -> Command {
    Command::cargo_bin("cybersin").unwrap()
}

#[cfg(unix)]
#[test]
fn sandbox_exec_runs_through_the_selected_backend() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("docker");
    fs::write(&runtime, "#!/bin/sh\nprintf 'hello from sandbox'\n").unwrap();
    let mut permissions = fs::metadata(&runtime).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&runtime, permissions).unwrap();

    cybersin()
        .env("CYBERSIN_CONTAINER_RUNTIME", &runtime)
        .args([
            "sandbox",
            "exec",
            "--backend",
            "docker",
            "--image",
            "example/tool:locked",
            "--root",
            temp.path().join("state").to_str().unwrap(),
            "--session",
            "session-1",
            "--call",
            "call-1",
            "--",
            "echo",
            "hello",
        ])
        .assert()
        .success()
        .stdout(predicate::eq("hello from sandbox"));
}

#[cfg(unix)]
#[test]
fn sandbox_snapshot_diff_and_restore_are_available_through_the_cli() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("docker");
    fs::write(
        &runtime,
        r#"#!/bin/sh
mount=""
previous=""
for argument in "$@"; do
  if [ "$previous" = "--mount" ]; then mount="$argument"; fi
  previous="$argument"
done
workspace="${mount#type=bind,src=}"
workspace="${workspace%,dst=/workspace}"
case " $* " in
  *" write-before "*) printf 'before' > "$workspace/state.txt" ;;
  *" write-after "*)
    printf 'after' > "$workspace/state.txt"
    printf 'created' > "$workspace/new.txt"
    ;;
esac
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&runtime).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&runtime, permissions).unwrap();
    let root = temp.path().join("state");
    let root_arg = root.to_str().unwrap();

    let exec = |call: &str, action: &str| {
        cybersin()
            .env("CYBERSIN_CONTAINER_RUNTIME", &runtime)
            .args([
                "sandbox",
                "exec",
                "--backend",
                "docker",
                "--image",
                "example/tool:locked",
                "--root",
                root_arg,
                "--scope",
                "session",
                "--session",
                "session-1",
                "--call",
                call,
                "--",
                action,
            ])
            .assert()
            .success();
    };

    exec("call-1", "write-before");
    cybersin()
        .args([
            "sandbox",
            "snapshot",
            "--root",
            root_arg,
            "--session",
            "session-1",
            "--checkpoint",
            "checkpoint-1",
        ])
        .assert()
        .success();

    exec("call-2", "write-after");
    cybersin()
        .args([
            "sandbox",
            "diff",
            "--root",
            root_arg,
            "--session",
            "session-1",
            "--checkpoint",
            "checkpoint-1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("A new.txt"))
        .stdout(predicate::str::contains("M state.txt"));

    cybersin()
        .args([
            "sandbox",
            "restore",
            "--root",
            root_arg,
            "--session",
            "session-1",
            "--checkpoint",
            "checkpoint-1",
        ])
        .assert()
        .success();
    cybersin()
        .args([
            "sandbox",
            "diff",
            "--root",
            root_arg,
            "--session",
            "session-1",
            "--checkpoint",
            "checkpoint-1",
        ])
        .assert()
        .success()
        .stdout(predicate::eq(""));
}
