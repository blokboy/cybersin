use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use cybersin_gateway::ToolExecutor;
use cybersin_runtime::DistFixture;
use cybersin_sandbox::{
    DockerBackend, ExecRequest, GvisorBackend, SandboxBackend, SandboxScope, WorkspaceStore,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct SandboxToolExecutor<B: ?Sized> {
    dist: Arc<DistFixture>,
    tool_assets: PathBuf,
    backend: Arc<B>,
    workspaces: Arc<WorkspaceStore>,
}

impl<B: ?Sized> SandboxToolExecutor<B> {
    pub fn new(
        dist: Arc<DistFixture>,
        tool_assets: PathBuf,
        backend: Arc<B>,
        workspaces: Arc<WorkspaceStore>,
    ) -> Self {
        Self {
            dist,
            tool_assets,
            backend,
            workspaces,
        }
    }
}

pub(crate) fn configured_executor(
    dist_dir: &Path,
    sandbox_root: &Path,
    backend_kind: crate::commands::sandbox::Backend,
) -> anyhow::Result<Arc<dyn ToolExecutor>> {
    let dist = Arc::new(DistFixture::load_dir(dist_dir)?);
    let workspaces = Arc::new(WorkspaceStore::new(sandbox_root)?);
    let binary = std::env::var_os("CYBERSIN_CONTAINER_RUNTIME").unwrap_or_else(|| "docker".into());
    let backend: Arc<dyn SandboxBackend + Send + Sync> = match backend_kind {
        crate::commands::sandbox::Backend::Docker => Arc::new(DockerBackend::with_binary(binary)),
        crate::commands::sandbox::Backend::DockerGvisor => {
            Arc::new(GvisorBackend::with_binary(binary))
        }
    };
    Ok(Arc::new(SandboxToolExecutor::new(
        dist,
        dist_dir.join("tools"),
        backend,
        workspaces,
    )))
}

#[async_trait]
impl<B> ToolExecutor for SandboxToolExecutor<B>
where
    B: SandboxBackend + Send + Sync + ?Sized + 'static,
{
    async fn execute(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let Some(policy) = self.dist.tool_policy(tool) else {
            return Err(format!(
                "unknown tool {tool:?} (not declared in any agent.yaml)"
            ));
        };
        if !policy.egress.is_empty() {
            return Err(format!(
                "tool {tool:?} declares sandbox.egress = {:?}, but egress allowlisting is not yet \
implemented — refusing to run with an ambiguous network posture",
                policy.egress
            ));
        }
        if policy.is_builtin() {
            return match tool {
                // TODO(issue #35): wire a real search provider.
                "web_search" => Err("web_search: no provider configured".into()),
                _ => Err(format!(
                    "tool {tool:?} is declared without `run` and has no built-in implementation"
                )),
            };
        }
        let command = policy
            .run
            .clone()
            .ok_or_else(|| format!("tool {tool:?} has no executable command"))?;
        let scope = policy.sandbox_scope()?;
        let session_id = sandbox_identifier(session_id);
        let call_id = sandbox_identifier(call_id);
        let session_lock = if scope == SandboxScope::Session {
            let workspaces = Arc::clone(&self.workspaces);
            let locked_session = session_id.clone();
            Some(
                tokio::task::spawn_blocking(move || workspaces.lock_session(&locked_session))
                    .await
                    .map_err(|error| {
                        format!("sandbox session-lock task for tool {tool:?} failed: {error}")
                    })?
                    .map_err(|error| {
                        format!("locking sandbox session for tool {tool:?}: {error}")
                    })?,
            )
        } else {
            None
        };
        let workspace = self
            .workspaces
            .open(scope, &session_id, &call_id)
            .map_err(|error| format!("opening sandbox workspace for tool {tool:?}: {error}"))?;
        if workspace.is_fresh()
            && !self.tool_assets.as_os_str().is_empty()
            && self.tool_assets.is_dir()
        {
            copy_asset_tree(&self.tool_assets, workspace.path())
                .map_err(|error| format!("seeding sandbox workspace for tool {tool:?}: {error}"))?;
        }
        let args_path = workspace.path().join("args.json");
        let encoded_args = serde_json::to_vec_pretty(args)
            .map_err(|error| format!("encoding arguments for tool {tool:?}: {error}"))?;
        replace_workspace_file(&args_path, &encoded_args)
            .map_err(|error| format!("writing {}: {error}", args_path.display()))?;

        let image = if policy.image.trim().is_empty() {
            "python:3.12-slim".to_string()
        } else {
            policy.image.clone()
        };
        let workspace_path = fs::canonicalize(workspace.path()).map_err(|error| {
            format!(
                "resolving sandbox workspace {} for tool {tool:?}: {error}",
                workspace.path().display()
            )
        })?;
        let request = ExecRequest {
            image,
            command,
            workspace: workspace_path,
            scope,
            egress: policy.egress.clone(),
            limits: policy.resource_limits(),
        };
        let backend = Arc::clone(&self.backend);
        let outcome = tokio::task::spawn_blocking(move || {
            let _session_lock = session_lock;
            let outcome = backend.exec(request);
            drop(workspace);
            outcome
        })
        .await
        .map_err(|error| format!("sandbox execution task for tool {tool:?} failed: {error}"))?
        .map_err(|error| format!("running sandbox for tool {tool:?}: {error}"))?;

        if !outcome.succeeded() {
            return Err(crate::commands::sandbox::outcome_failure_reason(&outcome));
        }
        Ok(serde_json::from_str(&outcome.stdout)
            .unwrap_or_else(|_| serde_json::json!({"stdout": outcome.stdout})))
    }
}

fn sandbox_identifier(raw: &str) -> String {
    if !raw.is_empty()
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return raw.to_string();
    }
    let readable: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let readable = readable.trim_matches('_');
    let readable = if readable.is_empty() { "id" } else { readable };
    let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
    format!("{readable}-{}", &digest[..12])
}

/// Replace one executor-owned workspace file without following anything
/// a previous session-scoped tool may have left at that path.
///
/// Session execution holds the workspace lock while this runs, so after
/// unlinking an existing file/symlink there is no sandbox process that
/// can race the `create_new` open.
fn replace_workspace_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(contents)
}

fn copy_asset_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination)
        .map_err(|error| format!("creating {}: {error}", destination.display()))?;
    let entries =
        fs::read_dir(source).map_err(|error| format!("reading {}: {error}", source.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("reading {}: {error}", source.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspecting {}: {error}", entry.path().display()))?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_asset_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)
                .map_err(|error| format!("copying {}: {error}", entry.path().display()))?;
        } else {
            return Err(format!(
                "tool assets may not contain symlinks or special files: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cybersin_runtime::{bundled_stub_dist_dir, ToolPolicy};
    use cybersin_sandbox::{ExecOutcome, ExecRequest, LimitKind, Termination};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct PanicBackend;

    impl SandboxBackend for PanicBackend {
        fn exec(&self, _request: ExecRequest) -> std::io::Result<ExecOutcome> {
            panic!("backend must not run")
        }
    }

    #[derive(Default)]
    struct InspectingBackend {
        requests: Mutex<Vec<ExecRequest>>,
    }

    impl SandboxBackend for InspectingBackend {
        fn exec(&self, request: ExecRequest) -> std::io::Result<ExecOutcome> {
            let args: Value =
                serde_json::from_slice(&std::fs::read(request.workspace.join("args.json"))?)?;
            let asset = std::fs::read_to_string(request.workspace.join("citation_lookup.py"))?;
            self.requests.lock().unwrap().push(request);
            Ok(ExecOutcome {
                exit_code: Some(0),
                stdout: serde_json::json!({"args": args, "asset": asset}).to_string(),
                stderr: String::new(),
                termination: Termination::Exited,
            })
        }
    }

    struct FixedBackend(ExecOutcome);

    impl SandboxBackend for FixedBackend {
        fn exec(&self, _request: ExecRequest) -> std::io::Result<ExecOutcome> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct StateBackend;

    impl SandboxBackend for StateBackend {
        fn exec(&self, request: ExecRequest) -> std::io::Result<ExecOutcome> {
            let state = request.workspace.join("state.txt");
            let seen_before = state.exists();
            std::fs::write(state, "persisted")?;
            Ok(ExecOutcome {
                exit_code: Some(0),
                stdout: serde_json::json!({"seen_before": seen_before}).to_string(),
                stderr: String::new(),
                termination: Termination::Exited,
            })
        }
    }

    #[derive(Default)]
    struct ConcurrentSessionBackend {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl SandboxBackend for ConcurrentSessionBackend {
        fn exec(&self, request: ExecRequest) -> std::io::Result<ExecOutcome> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(75));
            let args = std::fs::read_to_string(request.workspace.join("args.json"))?;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ExecOutcome {
                exit_code: Some(0),
                stdout: args,
                stderr: String::new(),
                termination: Termination::Exited,
            })
        }
    }

    #[cfg(unix)]
    struct SymlinkLeavingBackend {
        calls: AtomicUsize,
        victim: PathBuf,
    }

    #[cfg(unix)]
    impl SandboxBackend for SymlinkLeavingBackend {
        fn exec(&self, request: ExecRequest) -> std::io::Result<ExecOutcome> {
            let args_path = request.workspace.join("args.json");
            let args = std::fs::read_to_string(&args_path)?;
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::remove_file(&args_path)?;
                std::os::unix::fs::symlink(&self.victim, &args_path)?;
            }
            Ok(ExecOutcome {
                exit_code: Some(0),
                stdout: args,
                stderr: String::new(),
                termination: Termination::Exited,
            })
        }
    }

    fn policy(run: Option<Vec<&str>>, egress: Vec<&str>) -> ToolPolicy {
        ToolPolicy {
            retry_class: "read".into(),
            approval: None,
            image: "python:3.12-slim".into(),
            run: run.map(|parts| parts.into_iter().map(str::to_string).collect()),
            sandbox_scope: "call".into(),
            egress: egress.into_iter().map(str::to_string).collect(),
            cpu: 1.0,
            mem_mb: 512,
            wall_s: 30,
        }
    }

    fn executor(
        policies: impl IntoIterator<Item = (&'static str, ToolPolicy)>,
    ) -> (tempfile::TempDir, SandboxToolExecutor<PanicBackend>) {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        for (name, policy) in policies {
            dist.tools.insert(name.into(), policy);
        }
        let root = tempfile::tempdir().unwrap();
        let workspaces = Arc::new(WorkspaceStore::new(root.path()).unwrap());
        (
            root,
            SandboxToolExecutor::new(
                Arc::new(dist),
                PathBuf::new(),
                Arc::new(PanicBackend),
                workspaces,
            ),
        )
    }

    #[test]
    fn sandbox_tool_executor_is_the_concrete_gateway_executor() {
        fn assert_tool_executor<T: cybersin_gateway::ToolExecutor>() {}
        assert_tool_executor::<SandboxToolExecutor<cybersin_sandbox::DockerBackend>>();
    }

    #[tokio::test]
    async fn registry_distinguishes_unknown_tools_from_unconfigured_builtins() {
        let (_root, executor) = executor([("web_search", policy(None, vec![]))]);

        let unknown = executor
            .execute("session-1", "missing:k1", "missing", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            unknown,
            "unknown tool \"missing\" (not declared in any agent.yaml)"
        );

        let unconfigured = executor
            .execute(
                "session-1",
                "web_search:k1",
                "web_search",
                &serde_json::json!({"query": "cybernetics"}),
            )
            .await
            .unwrap_err();
        assert_eq!(unconfigured, "web_search: no provider configured");
    }

    #[tokio::test]
    async fn declared_egress_fails_closed_before_the_backend_runs() {
        let (_root, executor) = executor([(
            "citation_lookup",
            policy(
                Some(vec!["python3", "citation_lookup.py"]),
                vec!["api.example.com"],
            ),
        )]);

        let error = executor
            .execute(
                "session-1",
                "citation_lookup:k1",
                "citation_lookup",
                &serde_json::json!({"url": "https://example.com"}),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "tool \"citation_lookup\" declares sandbox.egress = [\"api.example.com\"], \
but egress allowlisting is not yet implemented — refusing to run with an ambiguous network posture"
        );
    }

    #[tokio::test]
    async fn custom_tool_runs_with_compiled_policy_assets_and_arguments() {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        let mut custom = policy(Some(vec!["python3", "citation_lookup.py"]), vec![]);
        custom.sandbox_scope = "call".into();
        custom.cpu = 0.5;
        custom.mem_mb = 64;
        custom.wall_s = 10;
        dist.tools.insert("citation_lookup".into(), custom);

        let state = tempfile::tempdir().unwrap();
        let assets = tempfile::tempdir().unwrap();
        std::fs::write(
            assets.path().join("citation_lookup.py"),
            "print('tool asset')\n",
        )
        .unwrap();
        let backend = Arc::new(InspectingBackend::default());
        let executor = SandboxToolExecutor::new(
            Arc::new(dist),
            assets.path().to_path_buf(),
            backend.clone(),
            Arc::new(WorkspaceStore::new(state.path()).unwrap()),
        );

        let result = executor
            .execute(
                "session:unsafe",
                "citation_lookup:key/1",
                "citation_lookup",
                &serde_json::json!({"citation": "C-1"}),
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            serde_json::json!({
                "args": {"citation": "C-1"},
                "asset": "print('tool asset')\n"
            })
        );
        let requests = backend.requests.lock().unwrap();
        let request = &requests[0];
        assert_eq!(request.image, "python:3.12-slim");
        assert_eq!(request.command, ["python3", "citation_lookup.py"]);
        assert_eq!(request.scope, SandboxScope::Call);
        assert!(request.egress.is_empty());
        assert_eq!(request.limits.cpus, 0.5);
        assert_eq!(request.limits.memory_mb, 64);
        assert_eq!(request.limits.pids, 128);
        assert_eq!(
            request.limits.wall_clock,
            std::time::Duration::from_secs(10)
        );
        assert!(
            !request.workspace.exists(),
            "call-scoped workspace must be discarded"
        );
    }

    #[tokio::test]
    async fn sandbox_failures_use_the_cli_container_error_contract() {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        dist.tools.insert(
            "citation_lookup".into(),
            policy(Some(vec!["python3", "citation_lookup.py"]), vec![]),
        );

        for (outcome, expected) in [
            (
                ExecOutcome {
                    exit_code: Some(7),
                    stdout: String::new(),
                    stderr: "boom\n".into(),
                    termination: Termination::Exited,
                },
                "container exited with Some(7): boom",
            ),
            (
                ExecOutcome {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    termination: Termination::KilledByLimit(LimitKind::WallClock),
                },
                "killed by WallClock limit",
            ),
        ] {
            let state = tempfile::tempdir().unwrap();
            let executor = SandboxToolExecutor::new(
                Arc::new(dist.clone()),
                PathBuf::new(),
                Arc::new(FixedBackend(outcome)),
                Arc::new(WorkspaceStore::new(state.path()).unwrap()),
            );
            let error = executor
                .execute(
                    "session-1",
                    "citation_lookup:k1",
                    "citation_lookup",
                    &serde_json::json!({}),
                )
                .await
                .unwrap_err();
            assert_eq!(error, expected);
        }
    }

    #[tokio::test]
    async fn session_scoped_workspace_persists_across_tool_calls() {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        let mut custom = policy(Some(vec!["python3", "citation_lookup.py"]), vec![]);
        custom.sandbox_scope = "session".into();
        dist.tools.insert("citation_lookup".into(), custom);
        let state = tempfile::tempdir().unwrap();
        let executor = SandboxToolExecutor::new(
            Arc::new(dist),
            PathBuf::new(),
            Arc::new(StateBackend),
            Arc::new(WorkspaceStore::new(state.path()).unwrap()),
        );

        let first = executor
            .execute(
                "session-1",
                "citation_lookup:k1",
                "citation_lookup",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        let second = executor
            .execute(
                "session-1",
                "citation_lookup:k2",
                "citation_lookup",
                &serde_json::json!({}),
            )
            .await
            .unwrap();

        assert_eq!(first, serde_json::json!({"seen_before": false}));
        assert_eq!(second, serde_json::json!({"seen_before": true}));
        assert!(state.path().join("workspaces/sessions/session-1").is_dir());
    }

    #[tokio::test]
    async fn concurrent_session_calls_are_serialized_and_keep_their_own_arguments() {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        let mut custom = policy(Some(vec!["python3", "citation_lookup.py"]), vec![]);
        custom.sandbox_scope = "session".into();
        dist.tools.insert("citation_lookup".into(), custom);
        let state = tempfile::tempdir().unwrap();
        let backend = Arc::new(ConcurrentSessionBackend::default());
        let executor = SandboxToolExecutor::new(
            Arc::new(dist),
            PathBuf::new(),
            backend.clone(),
            Arc::new(WorkspaceStore::new(state.path()).unwrap()),
        );
        let first_args = serde_json::json!({"call": "first"});
        let second_args = serde_json::json!({"call": "second"});

        let (first, second) = tokio::join!(
            executor.execute(
                "session-1",
                "citation_lookup:k1",
                "citation_lookup",
                &first_args,
            ),
            executor.execute(
                "session-1",
                "citation_lookup:k2",
                "citation_lookup",
                &second_args,
            ),
        );

        assert_eq!(first.unwrap(), first_args);
        assert_eq!(second.unwrap(), second_args);
        assert_eq!(
            backend.max_active.load(Ordering::SeqCst),
            1,
            "only one session-scoped sandbox may run at a time"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_tool_symlink_cannot_redirect_the_next_host_argument_write() {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        let mut custom = policy(Some(vec!["python3", "citation_lookup.py"]), vec![]);
        custom.sandbox_scope = "session".into();
        dist.tools.insert("citation_lookup".into(), custom);
        let state = tempfile::tempdir().unwrap();
        let victim = state.path().join("outside-workspace.json");
        std::fs::write(&victim, "do not overwrite").unwrap();
        let executor = SandboxToolExecutor::new(
            Arc::new(dist),
            PathBuf::new(),
            Arc::new(SymlinkLeavingBackend {
                calls: AtomicUsize::new(0),
                victim: victim.clone(),
            }),
            Arc::new(WorkspaceStore::new(state.path()).unwrap()),
        );

        executor
            .execute(
                "session-1",
                "citation_lookup:k1",
                "citation_lookup",
                &serde_json::json!({"call": "first"}),
            )
            .await
            .unwrap();
        let second = executor
            .execute(
                "session-1",
                "citation_lookup:k2",
                "citation_lookup",
                &serde_json::json!({"call": "second"}),
            )
            .await
            .unwrap();

        assert_eq!(second, serde_json::json!({"call": "second"}));
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "do not overwrite");
    }
}
