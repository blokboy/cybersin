use cybersin_sandbox::{
    DockerBackend, ExecRequest, LimitKind, ResourceLimits, SandboxBackend, SandboxScope,
    Termination,
};

fn request(workspace: &std::path::Path, command: &str) -> ExecRequest {
    ExecRequest {
        image: "alpine:3.20".into(),
        command: vec!["sh".into(), "-c".into(), command.into()],
        workspace: workspace.to_path_buf(),
        scope: SandboxScope::Session,
        limits: ResourceLimits {
            cpus: 0.5,
            memory_mb: 32,
            pids: 16,
            wall_clock: std::time::Duration::from_secs(2),
        },
    }
}

/// Requires a working local Docker daemon and the `alpine:3.20` image.
#[test]
#[ignore = "requires a live Docker daemon"]
fn exfiltration_and_process_exhaustion_are_contained_and_the_session_survives() {
    let workspace = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let backend = DockerBackend::new();

    let exfiltration = backend
        .exec(request(
            workspace.path(),
            "wget -q -T 1 -O /workspace/stolen https://example.com",
        ))
        .unwrap();
    assert!(!exfiltration.succeeded());
    assert!(!workspace.path().join("stolen").exists());

    let process_exhaustion = backend
        .exec(request(
            workspace.path(),
            "while :; do sh -c 'sleep 60' & done; wait",
        ))
        .unwrap();
    assert!(
        matches!(
            process_exhaustion.termination,
            Termination::KilledByLimit(LimitKind::WallClock | LimitKind::ContainerResource)
        ),
        "{process_exhaustion:?}"
    );

    let healthy_call = backend
        .exec(request(
            workspace.path(),
            "printf 'session alive'; printf alive > /workspace/healthy",
        ))
        .unwrap();
    assert!(healthy_call.succeeded());
    assert_eq!(healthy_call.stdout, "session alive");
    assert!(workspace.path().join("healthy").exists());
}
