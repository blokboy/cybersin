use std::fs;

use cybersin_sandbox::{DiffKind, SandboxScope, WorkspaceStore};

#[test]
fn session_workspace_can_diff_and_restore_a_checkpoint_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let store = WorkspaceStore::new(temp.path()).unwrap();
    let workspace = store
        .open(SandboxScope::Session, "session-1", "call-1")
        .unwrap();

    fs::write(workspace.path().join("state.txt"), "before").unwrap();
    workspace.snapshot("checkpoint-7").unwrap();

    fs::write(workspace.path().join("state.txt"), "after").unwrap();
    fs::write(workspace.path().join("new.txt"), "created").unwrap();

    assert_eq!(
        workspace.diff("checkpoint-7").unwrap(),
        vec![
            (std::path::PathBuf::from("new.txt"), DiffKind::Added),
            (std::path::PathBuf::from("state.txt"), DiffKind::Modified),
        ]
    );

    workspace.restore("checkpoint-7").unwrap();
    assert_eq!(
        fs::read_to_string(workspace.path().join("state.txt")).unwrap(),
        "before"
    );
    assert!(!workspace.path().join("new.txt").exists());
}

#[test]
fn call_workspaces_are_fresh_while_session_workspaces_persist() {
    let temp = tempfile::tempdir().unwrap();
    let store = WorkspaceStore::new(temp.path()).unwrap();

    let call_path = {
        let call = store
            .open(SandboxScope::Call, "session-1", "call-1")
            .unwrap();
        fs::write(call.path().join("call-only.txt"), "discard me").unwrap();
        call.path().to_path_buf()
    };
    assert!(!call_path.exists());

    {
        let session = store
            .open(SandboxScope::Session, "session-1", "call-1")
            .unwrap();
        fs::write(session.path().join("memory.txt"), "remember me").unwrap();
    }
    let resumed = store
        .open(SandboxScope::Session, "session-1", "call-2")
        .unwrap();
    assert_eq!(
        fs::read_to_string(resumed.path().join("memory.txt")).unwrap(),
        "remember me"
    );
}
