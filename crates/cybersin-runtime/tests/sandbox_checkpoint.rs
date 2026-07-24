use std::fs;
use std::sync::Arc;

use cybersin_adapter::stub_harness::StubHarness;
use cybersin_adapter::transport::stdio::in_memory_pair;
use cybersin_runtime::{
    bundled_stub_dist_dir, DistFixture, RuntimeDaemon, SessionSupervisor, SqliteStorage, Storage,
};
use cybersin_sandbox::{SandboxScope, WorkspaceStore};
use cybersin_trace::SpanStore;

#[tokio::test]
async fn session_resume_restores_the_workspace_snapshot_paired_with_its_checkpoint() {
    let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
    storage
        .create_session_pinned("session-1", "agent", "build-hash")
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let workspaces = WorkspaceStore::new(temp.path()).unwrap();
    let supervisor = SessionSupervisor::with_session_sandbox(storage, workspaces.clone());
    let workspace = workspaces
        .open(SandboxScope::Session, "session-1", "setup")
        .unwrap();

    fs::write(workspace.path().join("state.txt"), "checkpoint state").unwrap();
    supervisor
        .checkpoint("session-1", Some("manual"))
        .await
        .unwrap();
    fs::write(workspace.path().join("state.txt"), "uncheckpointed state").unwrap();

    supervisor.resume("session-1", "build-hash").await.unwrap();

    assert_eq!(
        fs::read_to_string(workspace.path().join("state.txt")).unwrap(),
        "checkpoint state"
    );
}

#[tokio::test]
async fn runtime_checkpoint_messages_snapshot_a_session_workspace() {
    let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
    let spans = SpanStore::in_memory().await.unwrap();
    let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());
    let build_hash = dist.manifest.build_hash.clone();
    let temp = tempfile::tempdir().unwrap();
    let workspaces = WorkspaceStore::new(temp.path()).unwrap();
    let workspace = workspaces
        .open(SandboxScope::Session, "session-2", "setup")
        .unwrap();
    let (harness_io, daemon_io) = in_memory_pair();
    let mut daemon = RuntimeDaemon::new(
        daemon_io,
        storage.clone(),
        spans,
        dist,
        "session-2",
        "agent",
    )
    .with_session_sandbox(workspaces.clone());
    daemon
        .start_session(serde_json::json!({"topic": "sandbox"}))
        .await
        .unwrap();
    let daemon_task = tokio::spawn(daemon.run());
    let mut harness = StubHarness::new(harness_io);
    harness.recv_session_start().await;

    fs::write(workspace.path().join("state.txt"), "at checkpoint").unwrap();
    harness.checkpoint(Some("manual".into())).await;
    fs::write(workspace.path().join("state.txt"), "after checkpoint").unwrap();
    harness
        .session_complete("session-2", serde_json::json!({"status": "ok"}))
        .await;
    harness.wait_for_close().await;
    daemon_task.await.unwrap().unwrap();

    SessionSupervisor::with_session_sandbox(storage, workspaces)
        .resume("session-2", &build_hash)
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.path().join("state.txt")).unwrap(),
        "at checkpoint"
    );
}
