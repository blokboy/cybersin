use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use cybersin_gateway::{GatewayOutcome, RetryClass, ToolExecutor, ToolGateway};
use cybersin_runtime::{DistFixture, LocalConfigFile, Storage};
use cybersin_sandbox::{
    DockerBackend, ExecRequest, GvisorBackend, SandboxBackend, SandboxScope, WorkspaceStore,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_TAVILY_BASE_URL: &str = "https://api.tavily.com";

/// One real implementation of a built-in tool (`web_search`, `web_fetch`,
/// ...) — the promotion path `SandboxToolExecutor::execute`'s dispatch
/// comment named ("deliberately not a trait-object registry, since there
/// are zero real built-in implementations to justify one yet; promote it
/// if/when one exists"). We now have two, so it's promoted.
#[async_trait]
trait BuiltinTool: Send + Sync {
    async fn call(&self, args: &Value) -> Result<Value, String>;
}

/// Shared HTTP glue for Tavily-backed built-ins (`web_search` ->
/// `/search`, `web_fetch` -> `/extract`) — same auth (`Bearer
/// TAVILY_API_KEY`), same client, same missing-key handling.
///
/// The key is read once at construction (`from_env`), not per call, but
/// its absence is deliberately *not* a construction-time error the way
/// `OpenRouterModelCaller::from_env` fails eagerly — these are optional
/// built-ins, not the model-calling backbone, so an agent that never
/// calls `web_search`/`web_fetch` must be completely unaffected by a
/// missing key. Each call site checks for the key itself via
/// `require_key` and fails clearly, without ever attempting a request,
/// when it's absent.
struct TavilyClient {
    http: reqwest::Client,
    api_key: Option<String>,
    base_url: String,
}

impl TavilyClient {
    fn new(api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            base_url: DEFAULT_TAVILY_BASE_URL.to_string(),
        }
    }

    fn from_env() -> Self {
        Self::new(std::env::var("TAVILY_API_KEY").ok())
    }

    fn from_local_config(config: Option<&LocalConfigFile>) -> Self {
        let Some(tool) = config.and_then(|config| config.tool("tavily")) else {
            return Self::from_env();
        };
        let api_key = tool
            .api_key
            .as_ref()
            .and_then(|reference| reference.read())
            .or_else(|| std::env::var("TAVILY_API_KEY").ok());
        let mut client = Self::new(api_key);
        if let Some(base_url) = &tool.base_url {
            client.base_url = base_url.clone();
        }
        client
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// `"{tool}: no provider configured"` — the exact error shape this
    /// crate used before either tool had a real implementation, preserved
    /// so the absence of a key degrades exactly like it always did.
    fn require_key(&self, tool: &str) -> Result<&str, String> {
        self.api_key
            .as_deref()
            .ok_or_else(|| format!("{tool}: no provider configured"))
    }
}

struct WebSearchTool(Arc<TavilyClient>);

#[async_trait]
impl BuiltinTool for WebSearchTool {
    async fn call(&self, args: &Value) -> Result<Value, String> {
        let api_key = self.0.require_key("web_search")?;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| "web_search: args.query must be a string".to_string())?;

        let response = self
            .0
            .http
            .post(format!("{}/search", self.0.base_url))
            .bearer_auth(api_key)
            .json(&serde_json::json!({ "query": query }))
            .send()
            .await
            .map_err(|error| format!("calling Tavily search: {error}"))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|error| format!("parsing Tavily search response: {error}"))?;
        if !status.is_success() {
            return Err(format!("Tavily search returned {status}: {payload}"));
        }

        Ok(serde_json::json!({
            "answer": payload.get("answer").cloned().unwrap_or(Value::Null),
            "results": payload.get("results").cloned().unwrap_or(Value::Array(Vec::new())),
        }))
    }
}

struct WebFetchTool(Arc<TavilyClient>);

#[async_trait]
impl BuiltinTool for WebFetchTool {
    async fn call(&self, args: &Value) -> Result<Value, String> {
        let api_key = self.0.require_key("web_fetch")?;
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| "web_fetch: args.url must be a string".to_string())?;

        let response = self
            .0
            .http
            .post(format!("{}/extract", self.0.base_url))
            .bearer_auth(api_key)
            .json(&serde_json::json!({ "urls": [url] }))
            .send()
            .await
            .map_err(|error| format!("calling Tavily extract: {error}"))?;
        let status = response.status();
        let payload: Value = response
            .json()
            .await
            .map_err(|error| format!("parsing Tavily extract response: {error}"))?;
        if !status.is_success() {
            return Err(format!("Tavily extract returned {status}: {payload}"));
        }

        if let Some(failure) = payload
            .get("failed_results")
            .and_then(Value::as_array)
            .and_then(|failures| failures.first())
        {
            let reason = failure
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!(
                "web_fetch: Tavily failed to extract {url:?}: {reason}"
            ));
        }
        let result = payload
            .get("results")
            .and_then(Value::as_array)
            .and_then(|results| results.first())
            .ok_or_else(|| format!("web_fetch: Tavily returned no result for {url:?}"))?;

        Ok(serde_json::json!({
            "url": result.get("url").cloned().unwrap_or(Value::String(url.to_string())),
            "content": result.get("raw_content").cloned().unwrap_or(Value::Null),
        }))
    }
}

fn default_builtins() -> HashMap<&'static str, Arc<dyn BuiltinTool>> {
    builtins_for(TavilyClient::from_env())
}

fn configured_builtins(
    config: Option<&LocalConfigFile>,
) -> HashMap<&'static str, Arc<dyn BuiltinTool>> {
    builtins_for(TavilyClient::from_local_config(config))
}

fn builtins_for(tavily: TavilyClient) -> HashMap<&'static str, Arc<dyn BuiltinTool>> {
    let tavily = Arc::new(tavily);
    let mut map: HashMap<&'static str, Arc<dyn BuiltinTool>> = HashMap::new();
    map.insert("web_search", Arc::new(WebSearchTool(tavily.clone())));
    map.insert("web_fetch", Arc::new(WebFetchTool(tavily)));
    map
}

pub struct SandboxToolExecutor<B: ?Sized> {
    dist: Arc<DistFixture>,
    tool_assets: PathBuf,
    backend: Arc<B>,
    workspaces: Arc<WorkspaceStore>,
    builtins: HashMap<&'static str, Arc<dyn BuiltinTool>>,
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
            builtins: default_builtins(),
        }
    }

    fn with_local_config(mut self, config: Option<&LocalConfigFile>) -> Self {
        self.builtins = configured_builtins(config);
        self
    }

    /// Test-only override of the Tavily-backed built-ins, so unit tests
    /// can point `web_search`/`web_fetch` at a `wiremock` server instead
    /// of a real, unconfigured, or absent `TAVILY_API_KEY`.
    #[cfg(test)]
    fn with_tavily(mut self, tavily: TavilyClient) -> Self {
        self.builtins = builtins_for(tavily);
        self
    }
}

pub(crate) fn configured_executor(
    dist_dir: &Path,
    sandbox_root: &Path,
    backend_kind: crate::commands::sandbox::Backend,
) -> anyhow::Result<Arc<dyn ToolExecutor>> {
    let project_dir = dist_dir.parent().unwrap_or_else(|| Path::new("."));
    let local_config = LocalConfigFile::load_optional(project_dir)?;
    configured_executor_with_local_config(
        dist_dir,
        sandbox_root,
        backend_kind,
        local_config.as_ref(),
    )
}

pub(crate) fn configured_executor_with_local_config(
    dist_dir: &Path,
    sandbox_root: &Path,
    backend_kind: crate::commands::sandbox::Backend,
    local_config: Option<&LocalConfigFile>,
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
    Ok(Arc::new(
        SandboxToolExecutor::new(dist, dist_dir.join("tools"), backend, workspaces)
            .with_local_config(local_config),
    ))
}

/// Bridges `cybersin_gateway::ToolGateway` -> `cybersin_runtime::ToolCaller`,
/// so `RuntimeDaemon`'s ungated tool-call path (`session.rs::
/// handle_tool_request`) runs through the same real, ledger-admitted,
/// retry-class-bounded gateway `dlq retry`/`approve`/`deny` already use
/// (issue #37), without `cybersin-runtime` depending on `cybersin-gateway`
/// (the reverse of that crate's normal dependency direction —
/// `cybersin-gateway` depends on `cybersin-runtime`, so only a crate that
/// depends on both, like this one, can bridge them).
///
/// No `PolicyHook` is ever registered on `self.gateway`, so
/// `ToolGateway::call` here can never return `GatewayOutcome::Parked` —
/// approval-gating stays entirely `RuntimeDaemon`'s own concern (its
/// `dist`-driven pre-check ahead of ever calling this bridge), independent
/// of which `ToolCaller` is attached. See `session.rs::handle_tool_request`'s
/// doc for why that decoupling matters.
pub(crate) struct GatewayToolCaller {
    gateway: ToolGateway,
    dist: Arc<DistFixture>,
}

impl GatewayToolCaller {
    pub(crate) fn new(
        executor: Arc<dyn ToolExecutor>,
        storage: Arc<dyn Storage>,
        dist: Arc<DistFixture>,
    ) -> Self {
        Self {
            gateway: ToolGateway::new(storage, executor),
            dist,
        }
    }
}

#[async_trait]
impl cybersin_runtime::ToolCaller for GatewayToolCaller {
    async fn call(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        args: &Value,
    ) -> Result<cybersin_runtime::ToolOutput, cybersin_runtime::ToolCallFailure> {
        let retry_class = self
            .dist
            .tool_policy(tool)
            .and_then(|policy| RetryClass::parse(&policy.retry_class))
            .unwrap_or(RetryClass::Write);
        // Session-scoped so the harness's own local call ids (e.g.
        // "call-2", not unique across sessions) can never collide in the
        // `(tool, idem_key)`-keyed ledger.
        let idem_key = format!("{session_id}:{call_id}");

        let outcome = self
            .gateway
            .call(session_id, tool, args.clone(), Some(idem_key), retry_class)
            .await
            .map_err(|error| cybersin_runtime::ToolCallFailure {
                // A gateway-level error (schema validation, storage) is
                // genuinely unexpected plumbing trouble, not a normal tool
                // failure — same "no basis to say don't retry" philosophy
                // this bridge already used before issue #37.
                reason: error.to_string(),
                retriable: true,
            })?;

        match outcome {
            GatewayOutcome::Resolved(cybersin_adapter::messages::CallOutcome::Ok { value }) => {
                Ok(cybersin_runtime::ToolOutput {
                    value,
                    // The real attempt count lives in the ledger row
                    // (`cybersin dlq show <call-id>`) but `GatewayOutcome`
                    // doesn't carry it back here — 0 mirrors this bridge's
                    // pre-issue-#37 behavior rather than inventing a
                    // number this call site can't see.
                    retries: 0,
                    // TODO(issue #35 follow-up): real per-tool cost
                    // metering. `cybersin_sandbox::ExecOutcome` carries no
                    // cost data today, so this mirrors the pre-Phase-3
                    // stub's flat placeholder rather than inventing false
                    // precision.
                    usd_cost: 0.0008,
                })
            }
            GatewayOutcome::Resolved(cybersin_adapter::messages::CallOutcome::Failed {
                reason,
                retriable,
            }) => Err(cybersin_runtime::ToolCallFailure { reason, retriable }),
            GatewayOutcome::Parked { .. } => {
                // Can't happen — see this struct's doc comment — but fail
                // closed rather than panic if it ever does.
                Err(cybersin_runtime::ToolCallFailure {
                    reason: format!(
                        "tool {tool:?} unexpectedly required approval on a live-session ungated \
call path"
                    ),
                    retriable: false,
                })
            }
        }
    }
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
            return match self.builtins.get(tool) {
                Some(builtin) => builtin.call(args).await,
                None => Err(format!(
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
        // Hermetic on purpose: `executor()` -> `SandboxToolExecutor::new`
        // builds its built-ins via `default_builtins()`, which reads
        // `TAVILY_API_KEY` from the ambient environment. That's always
        // absent in CI, but a developer machine genuinely configured with
        // a real key (e.g. for live-testing web_search itself) would
        // otherwise flip this test's "no provider configured" assertion
        // into a real network call. Force the key absent explicitly
        // rather than relying on the environment happening to agree.
        let (_root, executor) = executor([("web_search", policy(None, vec![]))]);
        let executor = executor.with_tavily(TavilyClient::new(None));

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

    #[test]
    fn tavily_placeholder_config_reads_referenced_environment_key() {
        std::env::set_var("CYBERSIN_TEST_TAVILY_KEY", "test-tavily-key");
        let config: LocalConfigFile = serde_yaml::from_str(
            r#"
tools:
  tavily:
    availability: auto
    api_key: ${CYBERSIN_TEST_TAVILY_KEY}
    base_url: https://tavily.local
"#,
        )
        .unwrap();

        let client = TavilyClient::from_local_config(Some(&config));

        assert_eq!(client.require_key("web_search").unwrap(), "test-tavily-key");
        assert_eq!(client.base_url, "https://tavily.local");
        std::env::remove_var("CYBERSIN_TEST_TAVILY_KEY");
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

    async fn in_memory_storage() -> Arc<dyn Storage> {
        Arc::new(cybersin_runtime::SqliteStorage::in_memory().await.unwrap())
    }

    #[derive(Default)]
    struct FlakyBackend {
        /// Number of leading calls that fail before this backend starts
        /// succeeding — lets a test prove the gateway's retry-class
        /// budget actually runs the executor again in-line.
        fails_first: usize,
        calls: AtomicUsize,
    }

    impl SandboxBackend for FlakyBackend {
        fn exec(&self, _request: ExecRequest) -> std::io::Result<ExecOutcome> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            if attempt < self.fails_first {
                return Ok(ExecOutcome {
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: "transient failure".into(),
                    termination: Termination::Exited,
                });
            }
            Ok(ExecOutcome {
                exit_code: Some(0),
                stdout: serde_json::json!({"ok": true}).to_string(),
                stderr: String::new(),
                termination: Termination::Exited,
            })
        }
    }

    struct AlwaysFailBackend;

    impl SandboxBackend for AlwaysFailBackend {
        fn exec(&self, _request: ExecRequest) -> std::io::Result<ExecOutcome> {
            Ok(ExecOutcome {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "permanent failure".into(),
                termination: Termination::Exited,
            })
        }
    }

    async fn gateway_tool_caller(
        tool: &'static str,
        tool_policy: ToolPolicy,
        backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> (tempfile::TempDir, Arc<dyn Storage>, GatewayToolCaller) {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        dist.tools.insert(tool.into(), tool_policy);
        let dist = Arc::new(dist);
        let state = tempfile::tempdir().unwrap();
        let inner = SandboxToolExecutor::new(
            dist.clone(),
            PathBuf::new(),
            Arc::new(backend),
            Arc::new(WorkspaceStore::new(state.path()).unwrap()),
        );
        let storage = in_memory_storage().await;
        let bridge = GatewayToolCaller::new(Arc::new(inner), storage.clone(), dist);
        (state, storage, bridge)
    }

    #[tokio::test]
    async fn gateway_tool_caller_maps_a_successful_execution() {
        let (_state, _storage, bridge) = gateway_tool_caller(
            "citation_lookup",
            policy(Some(vec!["python3", "citation_lookup.py"]), vec![]),
            FlakyBackend::default(),
        )
        .await;

        let output = cybersin_runtime::ToolCaller::call(
            &bridge,
            "session-1",
            "call-1",
            "citation_lookup",
            &serde_json::json!({}),
        )
        .await
        .unwrap();

        assert_eq!(output.value, serde_json::json!({"ok": true}));
        assert_eq!(output.usd_cost, 0.0008);
    }

    #[tokio::test]
    async fn gateway_tool_caller_propagates_a_failure_reason_unchanged() {
        let (_root, executor) = executor([]);
        let storage = in_memory_storage().await;
        let dist = Arc::new(DistFixture::load_dir(bundled_stub_dist_dir()).unwrap());
        let bridge = GatewayToolCaller::new(Arc::new(executor), storage, dist);

        let error = cybersin_runtime::ToolCaller::call(
            &bridge,
            "session-1",
            "call-1",
            "missing",
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.reason,
            "unknown tool \"missing\" (not declared in any agent.yaml)"
        );
        // A gateway-level error (this tool was never declared, so schema
        // validation never even runs) — no basis to say don't retry.
        assert!(error.retriable);
    }

    #[tokio::test]
    async fn gateway_tool_caller_admits_a_ledger_row_and_retries_within_budget() {
        let (_state, storage, bridge) = gateway_tool_caller(
            "citation_lookup",
            policy(Some(vec!["python3", "citation_lookup.py"]), vec![]),
            FlakyBackend {
                fails_first: 2,
                ..Default::default()
            },
        )
        .await;

        let output = cybersin_runtime::ToolCaller::call(
            &bridge,
            "session-1",
            "call-1",
            "citation_lookup",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(output.value, serde_json::json!({"ok": true}));

        // `retry_class: "read"` (from `policy()`'s helper default) allows
        // 3 auto-retries — the ledger row now exists (issue #37's whole
        // point: ungated live-session calls used to write no row at all)
        // and its attempt count proves the in-line retry loop actually
        // ran the executor 3 times before succeeding.
        let row = storage
            .get_tool_call("citation_lookup:session-1:call-1")
            .await
            .unwrap()
            .expect("gateway admitted a ledger row for this call");
        assert_eq!(row.status, "succeeded");
        assert_eq!(row.attempts, 3);
    }

    #[tokio::test]
    async fn gateway_tool_caller_marks_critical_failures_non_retriable() {
        let mut critical = policy(Some(vec!["python3", "citation_lookup.py"]), vec![]);
        critical.retry_class = "critical".into();
        let (_state, storage, bridge) =
            gateway_tool_caller("citation_lookup", critical, AlwaysFailBackend).await;

        let error = cybersin_runtime::ToolCaller::call(
            &bridge,
            "session-1",
            "call-1",
            "citation_lookup",
            &serde_json::json!({}),
        )
        .await
        .unwrap_err();
        assert!(!error.retriable);

        let row = storage
            .get_tool_call("citation_lookup:session-1:call-1")
            .await
            .unwrap()
            .expect("gateway admitted a ledger row for this call");
        assert_eq!(row.status, "failed");
        // `critical` never auto-retries: exactly one attempt.
        assert_eq!(row.attempts, 1);
    }

    fn executor_with_tavily(
        tavily: TavilyClient,
    ) -> (tempfile::TempDir, SandboxToolExecutor<PanicBackend>) {
        let mut dist = DistFixture::load_dir(bundled_stub_dist_dir()).unwrap();
        dist.tools.clear();
        dist.tools.insert("web_search".into(), policy(None, vec![]));
        dist.tools.insert("web_fetch".into(), policy(None, vec![]));
        let root = tempfile::tempdir().unwrap();
        let workspaces = Arc::new(WorkspaceStore::new(root.path()).unwrap());
        (
            root,
            SandboxToolExecutor::new(
                Arc::new(dist),
                PathBuf::new(),
                Arc::new(PanicBackend),
                workspaces,
            )
            .with_tavily(tavily),
        )
    }

    #[tokio::test]
    async fn web_search_calls_tavily_and_returns_results() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "query": "evidence-backed cybernetics"
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query": "evidence-backed cybernetics",
                "answer": "Cybernetics studies control and communication.",
                "results": [
                    {"title": "Cybernetics", "url": "https://example.com/cyb", "content": "...", "score": 0.9}
                ],
                "response_time": 0.1,
                "usage": {"credits": 1},
                "request_id": "req-1"
            })))
            .mount(&server)
            .await;

        let (_root, executor) = executor_with_tavily(
            TavilyClient::new(Some("test-key".into())).with_base_url(server.uri()),
        );

        let result = executor
            .execute(
                "session-1",
                "web_search:k1",
                "web_search",
                &serde_json::json!({"query": "evidence-backed cybernetics"}),
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            serde_json::json!({
                "answer": "Cybernetics studies control and communication.",
                "results": [
                    {"title": "Cybernetics", "url": "https://example.com/cyb", "content": "...", "score": 0.9}
                ],
            })
        );
    }

    #[tokio::test]
    async fn web_fetch_calls_tavily_extract_and_returns_content() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/extract"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "urls": ["https://example.com/cyb"]
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [
                        {"url": "https://example.com/cyb", "raw_content": "# Cybernetics\n..."}
                    ],
                    "failed_results": [],
                    "response_time": 0.1,
                    "usage": {"credits": 1},
                    "request_id": "req-2"
                })),
            )
            .mount(&server)
            .await;

        let (_root, executor) = executor_with_tavily(
            TavilyClient::new(Some("test-key".into())).with_base_url(server.uri()),
        );

        let result = executor
            .execute(
                "session-1",
                "web_fetch:k1",
                "web_fetch",
                &serde_json::json!({"url": "https://example.com/cyb"}),
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            serde_json::json!({
                "url": "https://example.com/cyb",
                "content": "# Cybernetics\n...",
            })
        );
    }

    #[tokio::test]
    async fn web_fetch_surfaces_a_clear_error_when_tavily_extraction_fails() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/extract"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "results": [],
                    "failed_results": [
                        {"url": "https://example.com/gone", "error": "404: page not found"}
                    ]
                })),
            )
            .mount(&server)
            .await;

        let (_root, executor) = executor_with_tavily(
            TavilyClient::new(Some("test-key".into())).with_base_url(server.uri()),
        );

        let error = executor
            .execute(
                "session-1",
                "web_fetch:k1",
                "web_fetch",
                &serde_json::json!({"url": "https://example.com/gone"}),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error,
            "web_fetch: Tavily failed to extract \"https://example.com/gone\": 404: page not found"
        );
    }

    #[tokio::test]
    async fn web_fetch_without_a_tavily_key_fails_clearly_before_any_network_call() {
        let (_root, executor) = executor_with_tavily(TavilyClient::new(None));

        let error = executor
            .execute(
                "session-1",
                "web_fetch:k1",
                "web_fetch",
                &serde_json::json!({"url": "https://example.com"}),
            )
            .await
            .unwrap_err();

        assert_eq!(error, "web_fetch: no provider configured");
    }
}
