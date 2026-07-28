//! Data-first capability metadata and execution events.
//!
//! The capability layer is intentionally separate from `commands::*`: command
//! modules are CLI adapters, while these types describe the shared product
//! surface that CLI and TUI adapters will eventually invoke.

use std::fs;
use std::path::{Path, PathBuf};

use cybersin_gateway::ToolGateway;
use cybersin_runtime::Storage;
use cybersin_trace::{CostDimension, CostRollupRow, Span, SpanFilter, SpanStore};
use serde_json::{json, Value};
use std::fmt::Write as _;

use crate::commands::build::{self, BuildProfile, BuildProgress};
use crate::commands::doctor;
use crate::commands::sandbox;
use crate::session_liveness::{display_liveness, heartbeat_display, holder_display, now_unix_ms};

pub const BUILD_CAPABILITY_ID: &str = "compile.build";
pub const CHECK_CAPABILITY_ID: &str = "compile.check";
pub const SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID: &str = "compile.scaffold-prompt-agent";
pub const SCAFFOLD_BUILD_WORKFLOW_ID: &str = "workflow.scaffold-build";
pub const TRACE_LS_CAPABILITY_ID: &str = "inspection.trace.ls";
pub const TRACE_SHOW_CAPABILITY_ID: &str = "inspection.trace.show";
pub const COST_CAPABILITY_ID: &str = "inspection.cost";
pub const DOCTOR_CAPABILITY_ID: &str = "inspection.doctor";
pub const SESSIONS_LS_CAPABILITY_ID: &str = "control.sessions.ls";
pub const SESSIONS_SHOW_CAPABILITY_ID: &str = "control.sessions.show";
pub const DLQ_LS_CAPABILITY_ID: &str = "control.dlq.ls";
pub const DLQ_SHOW_CAPABILITY_ID: &str = "control.dlq.show";
pub const SANDBOX_DIFF_CAPABILITY_ID: &str = "sandbox.diff";
pub const DIFF_CAPABILITY_ID: &str = "compile.diff";

/// A user-facing operation that can be invoked through one or more adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySpec {
    pub id: CapabilityId,
    pub title: String,
    pub summary: String,
    pub category: CapabilityCategory,
    pub components: Vec<CapabilityId>,
    pub input_schema: Value,
    pub output_modes: Vec<OutputMode>,
    pub safety: SafetyProfile,
    pub adapters: AdapterCoverage,
}

/// Stable, catalog-facing identifier for a capability.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Broad area of the Cybersin product surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityCategory {
    Compile,
    Runtime,
    Inspection,
    Control,
    Sandbox,
    Workflow,
}

/// Rendering styles a capability can emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Text,
    Json,
    Table,
    Tui,
    Artifact,
}

/// Safety metadata shared by CLI and TUI adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafetyProfile {
    pub file_mutation: MutationLevel,
    pub runtime_state_mutation: MutationLevel,
    pub process_lifecycle: ProcessLifecycle,
    pub network: NetworkRequirement,
    pub long_running: LongRunningBehavior,
    pub confirmation: ConfirmationPolicy,
}

impl SafetyProfile {
    pub fn read_only() -> Self {
        Self {
            file_mutation: MutationLevel::None,
            runtime_state_mutation: MutationLevel::None,
            process_lifecycle: ProcessLifecycle::None,
            network: NetworkRequirement::None,
            long_running: LongRunningBehavior::Finite,
            confirmation: ConfirmationPolicy::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MutationLevel {
    None,
    WritesProjectFiles,
    WritesRuntimeState,
    Destructive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessLifecycle {
    None,
    StartsChildProcess,
    StartsDaemon,
    ControlsExistingProcess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkRequirement {
    None,
    Optional,
    Required,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LongRunningBehavior {
    Finite,
    StreamsUntilComplete,
    WatchesUntilInterrupted,
    Daemon,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmationPolicy {
    None,
    Recommended,
    Required { reason: String },
}

/// Adapter availability for a capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCoverage {
    pub cli: AdapterSupport,
    pub tui: AdapterSupport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterSupport {
    Available,
    Generic,
    Custom,
    Unavailable { reason: String },
}

/// Event stream emitted by capability execution.
#[derive(Clone, Debug, PartialEq)]
pub enum CapabilityEvent {
    Started {
        capability_id: CapabilityId,
    },
    Progress {
        message: String,
        current: Option<u64>,
        total: Option<u64>,
    },
    Prompt {
        id: String,
        message: String,
        confirmation: ConfirmationPolicy,
    },
    Output {
        mode: OutputMode,
        value: Value,
    },
    Completed {
        value: Option<Value>,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckInput {
    pub path: PathBuf,
}

impl CheckInput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildInput {
    pub project_path: PathBuf,
    pub profile: BuildProfile,
    pub frozen: bool,
    pub selected_prompt_source: Option<PathBuf>,
}

impl BuildInput {
    pub fn new(project_path: impl Into<PathBuf>, profile: BuildProfile, frozen: bool) -> Self {
        Self {
            project_path: project_path.into(),
            profile,
            frozen,
            selected_prompt_source: None,
        }
    }

    pub fn with_selected_prompt_source(mut self, source: impl Into<PathBuf>) -> Self {
        self.selected_prompt_source = Some(source.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaffoldPromptAgentInput {
    pub project_path: PathBuf,
    pub prompt_source: PathBuf,
}

impl ScaffoldPromptAgentInput {
    pub fn new(project_path: impl Into<PathBuf>, prompt_source: impl Into<PathBuf>) -> Self {
        Self {
            project_path: project_path.into(),
            prompt_source: prompt_source.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaffoldPromptAgentOutput {
    pub prompt_name: String,
    pub agent_name: String,
    pub agent_path: PathBuf,
    pub harness_script_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScaffoldBuildInput {
    pub project_path: PathBuf,
    pub prompt_source: PathBuf,
    pub profile: BuildProfile,
    pub frozen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorInput {
    pub project_start: PathBuf,
}

impl DoctorInput {
    pub fn new(project_start: impl Into<PathBuf>) -> Self {
        Self {
            project_start: project_start.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CostInput {
    pub by: CostDimension,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceShowInput {
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionsLsInput {
    pub json: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionsShowInput {
    pub session: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DlqShowInput {
    pub call_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxDiffInput {
    pub root: PathBuf,
    pub session: String,
    pub checkpoint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffInput {
    pub project_path: PathBuf,
    pub reference: String,
    pub profile: BuildProfile,
}

impl ScaffoldBuildInput {
    pub fn new(
        project_path: impl Into<PathBuf>,
        prompt_source: impl Into<PathBuf>,
        profile: BuildProfile,
        frozen: bool,
    ) -> Self {
        Self {
            project_path: project_path.into(),
            prompt_source: prompt_source.into(),
            profile,
            frozen,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CapabilityExecution {
    pub events: Vec<CapabilityEvent>,
}

impl CapabilityExecution {
    pub fn new(events: Vec<CapabilityEvent>) -> Self {
        Self { events }
    }

    pub fn is_success(&self) -> bool {
        self.events
            .iter()
            .any(|event| matches!(event, CapabilityEvent::Completed { .. }))
    }
}

pub fn execute_build(input: BuildInput) -> CapabilityExecution {
    execute_build_with_progress(input, |_| {})
}

pub fn execute_build_with_progress(
    input: BuildInput,
    mut on_progress: impl FnMut(BuildProgress),
) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(BUILD_CAPABILITY_ID),
    }];

    let dist_dir = input.project_path.join("dist");
    let mut progress_events = Vec::new();
    let result = if let Some(source) = &input.selected_prompt_source {
        build::run_source_into_with_progress(
            &input.project_path,
            &dist_dir,
            source,
            input.profile,
            input.frozen,
            None,
            |progress| {
                on_progress(progress.clone());
                progress_events.push(build_progress_event(progress));
            },
        )
    } else {
        build::run_into_with_progress(
            &input.project_path,
            &dist_dir,
            input.profile,
            input.frozen,
            None,
            |progress| {
                on_progress(progress.clone());
                progress_events.push(build_progress_event(progress));
            },
        )
    };
    events.extend(progress_events);

    match result {
        Ok(message) => {
            let manifest = read_build_manifest(&dist_dir);
            let completed = json!({
                "project_path": input.project_path.display().to_string(),
                "dist_dir": dist_dir.display().to_string(),
                "profile": build_profile_name(input.profile),
                "frozen": input.frozen,
                "selected_prompt_source": input
                    .selected_prompt_source
                    .as_ref()
                    .map(|source| source.display().to_string()),
                "message": message,
                "build_hash": manifest
                    .as_ref()
                    .and_then(|value| value.get("build_hash"))
                    .and_then(Value::as_str),
                "artifacts": manifest
                    .as_ref()
                    .and_then(|value| value.get("artifacts"))
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            });
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Text,
                value: json!({
                    "stream": "stdout",
                    "text": completed
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("build complete"),
                }),
            });
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: completed.clone(),
            });
            events.push(CapabilityEvent::Completed {
                value: Some(completed),
            });
        }
        Err(message) => {
            events.push(CapabilityEvent::Failed { message });
        }
    }

    CapabilityExecution::new(events)
}

pub fn execute_scaffold_prompt_agent(input: ScaffoldPromptAgentInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID),
    }];

    match scaffold_prompt_agent(&input.project_path, &input.prompt_source) {
        Ok(output) => {
            let value = scaffold_prompt_agent_value(&output);
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: value.clone(),
            });
            events.push(CapabilityEvent::Completed { value: Some(value) });
        }
        Err(message) => events.push(CapabilityEvent::Failed { message }),
    }

    CapabilityExecution::new(events)
}

pub fn execute_scaffold_build(input: ScaffoldBuildInput) -> CapabilityExecution {
    execute_scaffold_build_with_progress(input, |_| {})
}

pub fn execute_scaffold_build_with_progress(
    input: ScaffoldBuildInput,
    on_progress: impl FnMut(BuildProgress),
) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(SCAFFOLD_BUILD_WORKFLOW_ID),
    }];

    let scaffold = match scaffold_prompt_agent(&input.project_path, &input.prompt_source) {
        Ok(output) => output,
        Err(message) => {
            events.push(CapabilityEvent::Started {
                capability_id: CapabilityId::new(SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID),
            });
            events.push(CapabilityEvent::Failed {
                message: message.clone(),
            });
            events.push(CapabilityEvent::Failed { message });
            return CapabilityExecution::new(events);
        }
    };
    let scaffold_value = scaffold_prompt_agent_value(&scaffold);
    events.push(CapabilityEvent::Started {
        capability_id: CapabilityId::new(SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID),
    });
    events.push(CapabilityEvent::Output {
        mode: OutputMode::Json,
        value: scaffold_value.clone(),
    });
    events.push(CapabilityEvent::Completed {
        value: Some(scaffold_value.clone()),
    });

    let build = execute_build_with_progress(
        BuildInput::new(&input.project_path, input.profile, input.frozen)
            .with_selected_prompt_source(&input.prompt_source),
        on_progress,
    );
    let build_success = build.is_success();
    let build_value = build.events.iter().rev().find_map(|event| match event {
        CapabilityEvent::Completed { value } => value.clone(),
        _ => None,
    });
    events.extend(build.events);

    if build_success {
        let build_message = build_value
            .as_ref()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("build complete");
        let agent_path = display_project_path(&input.project_path, &scaffold.agent_path);
        let message = format!("{build_message}; agent {agent_path}");
        events.push(CapabilityEvent::Output {
            mode: OutputMode::Text,
            value: json!({
                "stream": "stdout",
                "text": message,
            }),
        });
        events.push(CapabilityEvent::Completed {
            value: Some(json!({
                "scaffold": scaffold_value,
                "build": build_value,
                "message": message,
            })),
        });
    } else if let Some(Err(message)) = build_summary(&events) {
        events.push(CapabilityEvent::Failed { message });
    }

    CapabilityExecution::new(events)
}

#[derive(Debug, serde::Deserialize)]
struct PromptNameYaml {
    name: String,
}

#[derive(Debug, serde::Deserialize)]
struct AgentNameYaml {
    name: String,
}

fn scaffold_prompt_agent(
    project_path: &Path,
    prompt_source: &Path,
) -> Result<ScaffoldPromptAgentOutput, String> {
    let prompt_text = fs::read_to_string(prompt_source)
        .map_err(|e| format!("error: failed to read {}: {e}", prompt_source.display()))?;
    let prompt: PromptNameYaml = serde_yaml::from_str(&prompt_text)
        .map_err(|e| format!("error: invalid {}: {e}", prompt_source.display()))?;
    let prompt_slug = prompt_name_slug(&prompt.name);
    let agent_name = format!("{prompt_slug}-agent");
    let generated_agent_path = project_path
        .join("agents")
        .join(format!("{prompt_slug}.agent.yaml"));

    for agent_source in build::discover_agent_sources(project_path)? {
        if agent_source == generated_agent_path {
            continue;
        }
        let text = fs::read_to_string(&agent_source)
            .map_err(|e| format!("error: failed to read {}: {e}", agent_source.display()))?;
        let agent: AgentNameYaml = serde_yaml::from_str(&text)
            .map_err(|e| format!("error: invalid {}: {e}", agent_source.display()))?;
        if agent.name == agent_name {
            let script_path = project_path
                .join("harnesses")
                .join(format!("{prompt_slug}.script.yaml"));
            return Ok(ScaffoldPromptAgentOutput {
                prompt_name: prompt.name,
                agent_name,
                agent_path: agent_source,
                harness_script_path: script_path,
            });
        }
    }

    let script_path = project_path
        .join("harnesses")
        .join(format!("{prompt_slug}.script.yaml"));
    write_prompt_harness_script(&script_path, &prompt.name)?;

    if let Some(parent) = generated_agent_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("error: failed to create {}: {e}", parent.display()))?;
    }
    let script_rel = display_project_path(project_path, &script_path);
    fs::write(
        &generated_agent_path,
        scaffold_agent_yaml(&agent_name, &script_rel),
    )
    .map_err(|e| {
        format!(
            "error: failed to write {}: {e}",
            generated_agent_path.display()
        )
    })?;

    Ok(ScaffoldPromptAgentOutput {
        prompt_name: prompt.name,
        agent_name,
        agent_path: generated_agent_path,
        harness_script_path: script_path,
    })
}

fn scaffold_prompt_agent_value(output: &ScaffoldPromptAgentOutput) -> Value {
    json!({
        "prompt_name": output.prompt_name,
        "agent_name": output.agent_name,
        "agent_path": output.agent_path.display().to_string(),
        "harness_script_path": output.harness_script_path.display().to_string(),
    })
}

fn prompt_name_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "prompt".to_string()
    } else {
        slug
    }
}

fn write_prompt_harness_script(path: &Path, prompt_name: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("error: failed to create {}: {e}", parent.display()))?;
    }
    let prompt_name = serde_json::to_string(prompt_name)
        .map_err(|e| format!("error: failed to serialize prompt name: {e}"))?;
    let text = format!(
        "- llm_request:\n\
    prompt_name: {prompt_name}\n\
    inputs: {{}}\n"
    );
    fs::write(path, text).map_err(|e| format!("error: failed to write {}: {e}", path.display()))
}

fn scaffold_agent_yaml(agent_name: &str, script_path: &str) -> String {
    let script_path = serde_json::to_string(script_path).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "# Agent runtime config generated by the Cybersin TUI Build tab.\n\
name: {agent_name}\n\
harness: {{ adapter: process, command: [\"scripted_harness\", {script_path}] }}\n\
budget: {{ usd_per_session: 1.00, on_breach: degrade }}\n\
tools: []\n"
    )
}

fn display_project_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub fn build_summary(events: &[CapabilityEvent]) -> Option<Result<String, String>> {
    for event in events.iter().rev() {
        match event {
            CapabilityEvent::Completed {
                value: Some(value), ..
            } => {
                let message = value.get("message")?;
                if message.is_null() {
                    return Some(Ok("build complete".to_string()));
                }
                return Some(Ok(message.as_str()?.to_string()));
            }
            CapabilityEvent::Failed { message } => return Some(Err(message.clone())),
            _ => {}
        }
    }
    None
}

fn build_progress_event(progress: BuildProgress) -> CapabilityEvent {
    match progress {
        BuildProgress::DiscoveredPrompts(sources) => CapabilityEvent::Progress {
            message: format!("discovered {} prompt source(s)", sources.len()),
            current: Some(0),
            total: Some(sources.len() as u64),
        },
        BuildProgress::ClearingDist(path) => CapabilityEvent::Progress {
            message: format!("clearing {}", path.display()),
            current: None,
            total: None,
        },
        BuildProgress::PromptStarted { name, source } => CapabilityEvent::Progress {
            message: format!("building {name} from {}", source.display()),
            current: None,
            total: None,
        },
        BuildProgress::PassFinished { prompt, pass } => CapabilityEvent::Progress {
            message: format!("{prompt}: {pass} pass finished"),
            current: None,
            total: None,
        },
        BuildProgress::PromptWritten(prompt) => CapabilityEvent::Progress {
            message: format!("wrote prompt artifact for {prompt}"),
            current: None,
            total: None,
        },
        BuildProgress::Routing => CapabilityEvent::Progress {
            message: "wrote routing artifact".to_string(),
            current: None,
            total: None,
        },
        BuildProgress::Cache => CapabilityEvent::Progress {
            message: "wrote cache seed".to_string(),
            current: None,
            total: None,
        },
        BuildProgress::Tools => CapabilityEvent::Progress {
            message: "wrote tool policy artifacts".to_string(),
            current: None,
            total: None,
        },
        BuildProgress::Manifest => CapabilityEvent::Progress {
            message: "wrote build manifest".to_string(),
            current: None,
            total: None,
        },
    }
}

fn read_build_manifest(dist_dir: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(dist_dir.join("manifest.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn build_profile_name(profile: BuildProfile) -> &'static str {
    match profile {
        BuildProfile::Dev => "dev",
        BuildProfile::Release => "release",
    }
}

pub fn execute_doctor(input: DoctorInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(DOCTOR_CAPABILITY_ID),
    }];

    match doctor::diagnose(&input.project_start) {
        Ok(report) => {
            let text = format!("{}\n", report.render());
            events.push(text_output("stdout", text));
            let value = json!({
                "project_start": input.project_start.display().to_string(),
                "ok": report.ok,
            });
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: value.clone(),
            });
            if report.ok {
                events.push(CapabilityEvent::Completed { value: Some(value) });
            } else {
                events.push(CapabilityEvent::Failed {
                    message: "project is not ready; see next actions above".to_string(),
                });
            }
        }
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub async fn execute_cost(spans: &SpanStore, input: CostInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(COST_CAPABILITY_ID),
    }];

    match spans.cost_rollup(input.by).await {
        Ok(rows) => {
            events.push(text_output("stdout", render_cost_text(input.by, &rows)));
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: json!({
                    "by": input.by,
                    "rows": rows,
                }),
            });
            events.push(CapabilityEvent::Completed {
                value: Some(json!({ "rows": rows.len() })),
            });
        }
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub async fn execute_trace_show(spans: &SpanStore, input: TraceShowInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(TRACE_SHOW_CAPABILITY_ID),
    }];

    match spans.get(&input.id).await {
        Ok(Some(span)) => {
            let value = serde_json::to_value(&span).expect("Span should serialize");
            let text =
                serde_json::to_string_pretty(&span).expect("Span should serialize to pretty JSON");
            events.push(text_output("stdout", format!("{text}\n")));
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: value.clone(),
            });
            events.push(CapabilityEvent::Completed { value: Some(value) });
        }
        Ok(None) => events.push(CapabilityEvent::Failed {
            message: format!("no span with id {:?}", input.id),
        }),
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub async fn execute_sessions_ls(
    storage: &dyn Storage,
    input: SessionsLsInput,
) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(SESSIONS_LS_CAPABILITY_ID),
    }];

    match storage.list_sessions().await {
        Ok(sessions) => {
            let now = now_unix_ms();
            let rows: Vec<_> = sessions
                .iter()
                .map(|s| {
                    json!({
                        "session_id": s.session_id,
                        "status": s.status,
                        "agent_name": s.agent_name,
                        "config_hash": s.config_hash,
                        "created_unix_ms": s.created_unix_ms,
                        "last_heartbeat_unix_ms": s.last_heartbeat_unix_ms,
                        "heartbeat_holder": s.heartbeat_holder,
                        "liveness": display_liveness(s, now),
                    })
                })
                .collect();
            let text = if input.json {
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&rows).expect("session rows should serialize")
                )
            } else {
                render_sessions_ls_text(&sessions, now)
            };
            events.push(text_output("stdout", text));
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: json!({ "sessions": rows }),
            });
            events.push(CapabilityEvent::Completed {
                value: Some(json!({ "sessions": sessions.len() })),
            });
        }
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub async fn execute_sessions_show(
    storage: &dyn Storage,
    input: SessionsShowInput,
) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(SESSIONS_SHOW_CAPABILITY_ID),
    }];

    match storage.get_session(&input.session).await {
        Ok(Some(s)) => {
            let events_log = match storage.load_events(&input.session).await {
                Ok(events_log) => events_log,
                Err(error) => {
                    events.push(CapabilityEvent::Failed {
                        message: error.to_string(),
                    });
                    return CapabilityExecution::new(events);
                }
            };
            let state = match storage.list_state(&input.session).await {
                Ok(state) => state,
                Err(error) => {
                    events.push(CapabilityEvent::Failed {
                        message: error.to_string(),
                    });
                    return CapabilityExecution::new(events);
                }
            };
            let checkpoint = match storage.latest_checkpoint(&input.session).await {
                Ok(checkpoint) => checkpoint,
                Err(error) => {
                    events.push(CapabilityEvent::Failed {
                        message: error.to_string(),
                    });
                    return CapabilityExecution::new(events);
                }
            };
            let value = json!({
                "session_id": s.session_id,
                "agent_name": s.agent_name,
                "status": s.status,
                "config_hash": s.config_hash,
                "created_unix_ms": s.created_unix_ms,
                "last_heartbeat_unix_ms": s.last_heartbeat_unix_ms,
                "heartbeat_holder": s.heartbeat_holder,
                "liveness": display_liveness(&s, now_unix_ms()),
                "events": events_log,
                "state": state,
                "checkpoint": checkpoint,
            });
            let text =
                serde_json::to_string_pretty(&value).expect("session detail should serialize");
            events.push(text_output("stdout", format!("{text}\n")));
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: value.clone(),
            });
            events.push(CapabilityEvent::Completed { value: Some(value) });
        }
        Ok(None) => events.push(CapabilityEvent::Failed {
            message: format!("session {:?} not found", input.session),
        }),
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub async fn execute_dlq_ls(gateway: &ToolGateway) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(DLQ_LS_CAPABILITY_ID),
    }];

    match gateway.dlq_list().await {
        Ok(rows) => {
            events.push(text_output("stdout", render_dlq_ls_text(&rows)));
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: json!({ "dead_letters": rows }),
            });
            events.push(CapabilityEvent::Completed {
                value: Some(json!({ "dead_letters": rows.len() })),
            });
        }
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub async fn execute_dlq_show(gateway: &ToolGateway, input: DlqShowInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(DLQ_SHOW_CAPABILITY_ID),
    }];

    match gateway.dlq_show(&input.call_id).await {
        Ok(row) => {
            let value = serde_json::to_value(&row).expect("ToolCallRecord should serialize");
            let text = serde_json::to_string_pretty(&row)
                .expect("ToolCallRecord should serialize to pretty JSON");
            events.push(text_output("stdout", format!("{text}\n")));
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: value.clone(),
            });
            events.push(CapabilityEvent::Completed { value: Some(value) });
        }
        Err(error) => events.push(CapabilityEvent::Failed {
            message: error.to_string(),
        }),
    }

    CapabilityExecution::new(events)
}

pub fn execute_sandbox_diff(input: SandboxDiffInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(SANDBOX_DIFF_CAPABILITY_ID),
    }];

    let args = sandbox::LifecycleArgs {
        root: input.root,
        session: input.session,
        checkpoint: input.checkpoint,
    };
    match sandbox::diff_text(&args) {
        Ok(text) => {
            events.push(text_output("stdout", text));
            events.push(CapabilityEvent::Completed {
                value: Some(json!({ "ok": true })),
            });
        }
        Err(message) => events.push(CapabilityEvent::Failed { message }),
    }

    CapabilityExecution::new(events)
}

pub fn execute_diff(input: DiffInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(DIFF_CAPABILITY_ID),
    }];

    match crate::commands::diff::run_with_output(
        &input.project_path,
        &input.reference,
        input.profile,
    ) {
        Ok(output) => {
            if !output.text.is_empty() {
                events.push(text_output("stdout", output.text));
            }
            let value = json!({
                "project_path": input.project_path.display().to_string(),
                "reference": input.reference,
                "profile": build_profile_name(input.profile),
                "message": output.summary,
            });
            events.push(CapabilityEvent::Output {
                mode: OutputMode::Json,
                value: value.clone(),
            });
            events.push(CapabilityEvent::Completed { value: Some(value) });
        }
        Err(message) => events.push(CapabilityEvent::Failed { message }),
    }

    CapabilityExecution::new(events)
}

pub fn simple_result(events: &[CapabilityEvent]) -> Option<Result<(), String>> {
    for event in events.iter().rev() {
        match event {
            CapabilityEvent::Completed { .. } => return Some(Ok(())),
            CapabilityEvent::Failed { message } => return Some(Err(message.clone())),
            _ => {}
        }
    }
    None
}

pub fn rendered_text(events: &[CapabilityEvent]) -> String {
    let mut rendered = String::new();
    for event in events {
        if let CapabilityEvent::Output {
            mode: OutputMode::Text,
            value,
        } = event
        {
            if let Some((stream, text)) = output_stream(value) {
                if stream == "stdout" {
                    rendered.push_str(text);
                }
            }
        }
    }
    rendered
}

pub fn output_stream(value: &Value) -> Option<(&str, &str)> {
    check_output_stream(value)
}

fn text_output(stream: &str, text: String) -> CapabilityEvent {
    CapabilityEvent::Output {
        mode: OutputMode::Text,
        value: json!({
            "stream": stream,
            "text": text,
        }),
    }
}

fn render_cost_text(dimension: CostDimension, rows: &[CostRollupRow]) -> String {
    if rows.is_empty() {
        return "no cost data yet — try `cybersin run --stub` first\n".to_string();
    }

    let mut text = String::new();
    writeln!(
        &mut text,
        "{:<24} {:>10} {:>8} {:>10} {:>10}",
        dimension.to_string().to_uppercase(),
        "USD",
        "SPANS",
        "PTOK",
        "CTOK"
    )
    .expect("writing to String should not fail");
    let mut total_usd = 0.0;
    for row in rows {
        writeln!(
            &mut text,
            "{:<24} {:>10.6} {:>8} {:>10} {:>10}",
            row.key, row.usd_cost, row.span_count, row.tokens_prompt, row.tokens_completion
        )
        .expect("writing to String should not fail");
        total_usd += row.usd_cost;
    }
    writeln!(&mut text, "{:<24} {:>10.6}", "TOTAL", total_usd)
        .expect("writing to String should not fail");
    text
}

fn render_sessions_ls_text(
    sessions: &[cybersin_runtime::storage::SessionRecord],
    now: i64,
) -> String {
    let mut text = String::new();
    for s in sessions {
        writeln!(
            &mut text,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            s.session_id,
            s.status,
            s.agent_name,
            s.config_hash,
            display_liveness(s, now),
            heartbeat_display(s),
            holder_display(s)
        )
        .expect("writing to String should not fail");
    }
    text
}

fn render_dlq_ls_text(rows: &[cybersin_runtime::storage::ToolCallRecord]) -> String {
    if rows.is_empty() {
        return "no dead letters\n".to_string();
    }

    let mut text = String::new();
    writeln!(
        &mut text,
        "{:<28} {:<12} {:<10} {:>8} {:<20}",
        "CALL_ID", "SESSION", "CLASS", "ATTEMPTS", "REASON"
    )
    .expect("writing to String should not fail");
    for row in rows {
        writeln!(
            &mut text,
            "{:<28} {:<12} {:<10} {:>8} {:<20}",
            row.call_id,
            row.session_id,
            row.retry_class,
            row.attempts,
            row.failure_reason.as_deref().unwrap_or("-"),
        )
        .expect("writing to String should not fail");
    }
    text
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceLsInput {
    pub session: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub limit: Option<u32>,
}

impl TraceLsInput {
    pub fn filter(&self) -> SpanFilter {
        SpanFilter {
            session_id: self.session.clone(),
            agent_name: self.agent.clone(),
            kind: None,
            model: self.model.clone(),
            since_unix_ms: None,
            limit: self.limit,
        }
    }
}

pub async fn execute_trace_ls(spans: &SpanStore, input: TraceLsInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(TRACE_LS_CAPABILITY_ID),
    }];

    let listed = match spans.list(&input.filter()).await {
        Ok(spans) => spans,
        Err(e) => {
            events.push(CapabilityEvent::Failed {
                message: e.to_string(),
            });
            return CapabilityExecution::new(events);
        }
    };

    events.push(CapabilityEvent::Output {
        mode: OutputMode::Text,
        value: json!({
            "stream": "stdout",
            "text": render_trace_ls_text(&listed),
        }),
    });
    events.push(CapabilityEvent::Output {
        mode: OutputMode::Json,
        value: json!({
            "spans": listed,
        }),
    });
    events.push(CapabilityEvent::Completed {
        value: Some(json!({
            "spans": listed.len(),
        })),
    });

    CapabilityExecution::new(events)
}

pub fn trace_ls_result(events: &[CapabilityEvent]) -> Option<Result<(), String>> {
    for event in events.iter().rev() {
        match event {
            CapabilityEvent::Completed { .. } => return Some(Ok(())),
            CapabilityEvent::Failed { message } => return Some(Err(message.clone())),
            _ => {}
        }
    }
    None
}

pub fn trace_ls_output_stream(value: &Value) -> Option<(&str, &str)> {
    check_output_stream(value)
}

fn render_trace_ls_text(spans: &[Span]) -> String {
    if spans.is_empty() {
        return "no spans recorded yet — try `cybersin run --stub` first\n".to_string();
    }

    let mut text = String::new();
    writeln!(
        &mut text,
        "{:<24} {:<14} {:<16} {:<16} {:>6} {:>6} {:>10} {:<8}",
        "ID", "KIND", "NAME", "MODEL", "PTOK", "CTOK", "USD", "CACHE"
    )
    .expect("writing to String should not fail");
    for span in spans {
        writeln!(
            &mut text,
            "{:<24} {:<14} {:<16} {:<16} {:>6} {:>6} {:>10.6} {:<8}",
            span.id,
            span.kind.as_str(),
            span.name,
            span.model.as_deref().unwrap_or("-"),
            span.tokens_prompt
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
            span.tokens_completion
                .map(|t| t.to_string())
                .unwrap_or_else(|| "-".to_string()),
            span.usd_cost,
            span.cache_status.as_str(),
        )
        .expect("writing to String should not fail");
    }
    text
}

pub fn execute_check(input: CheckInput) -> CapabilityExecution {
    let mut events = vec![CapabilityEvent::Started {
        capability_id: CapabilityId::new(CHECK_CAPABILITY_ID),
    }];

    let sources = match cybersin_frontend::discover_prompt_sources(&input.path) {
        Ok(sources) => sources,
        Err(e) => {
            events.push(CapabilityEvent::Failed {
                message: format!("error: could not read {}: {e}", input.path.display()),
            });
            return CapabilityExecution::new(events);
        }
    };

    if sources.is_empty() {
        events.push(CapabilityEvent::Failed {
            message: format!(
                "error: no *.prompt.yaml sources found at {}",
                input.path.display()
            ),
        });
        return CapabilityExecution::new(events);
    }

    let total = sources.len();
    let mut failed = 0usize;
    let mut results = Vec::new();
    for source in &sources {
        match cybersin_frontend::compile_prompt_source(source) {
            Ok(ir) => {
                events.push(check_text_output(
                    "stdout",
                    format!("ok    {}", source.display()),
                ));
                let ir = serde_json::to_value(ir).expect("PromptIr should serialize");
                events.push(CapabilityEvent::Output {
                    mode: OutputMode::Json,
                    value: json!({
                        "path": source.display().to_string(),
                        "status": "ok",
                        "ir": ir,
                    }),
                });
                results.push(json!({
                    "path": source.display().to_string(),
                    "status": "ok",
                    "ir": ir,
                }));
            }
            Err(e) => {
                failed += 1;
                let error = e.to_string();
                events.push(check_text_output(
                    "stderr",
                    format!("FAIL  {}\n{error}\n", source.display()),
                ));
                results.push(json!({
                    "path": source.display().to_string(),
                    "status": "failed",
                    "error": error,
                }));
            }
        }
    }

    let result = json!({
        "path": input.path.display().to_string(),
        "sources": total,
        "failed": failed,
        "results": results,
    });

    if failed == 0 {
        events.push(CapabilityEvent::Completed {
            value: Some(result),
        });
    } else {
        events.push(CapabilityEvent::Failed {
            message: format!("cybersin check failed: {failed} of {total} source(s) had errors"),
        });
    }

    CapabilityExecution::new(events)
}

pub fn check_summary(events: &[CapabilityEvent]) -> Option<Result<String, String>> {
    for event in events.iter().rev() {
        match event {
            CapabilityEvent::Completed {
                value: Some(value), ..
            } => {
                let sources = value.get("sources")?.as_u64()?;
                return Some(Ok(format!("cybersin check: {sources} source(s) ok")));
            }
            CapabilityEvent::Failed { message } => return Some(Err(message.clone())),
            _ => {}
        }
    }
    None
}

pub fn check_output_stream(value: &Value) -> Option<(&str, &str)> {
    let stream = value.get("stream")?.as_str()?;
    let text = value.get("text")?.as_str()?;
    Some((stream, text))
}

fn check_text_output(stream: &str, text: String) -> CapabilityEvent {
    CapabilityEvent::Output {
        mode: OutputMode::Text,
        value: json!({
            "stream": stream,
            "text": text,
        }),
    }
}

/// Data-only catalog of Cybersin's current user-facing product surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityRegistry {
    specs: Vec<CapabilitySpec>,
}

impl CapabilityRegistry {
    pub fn new(specs: Vec<CapabilitySpec>) -> Self {
        Self { specs }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn specs(&self) -> &[CapabilitySpec] {
        &self.specs
    }

    pub fn get(&self, id: &str) -> Option<&CapabilitySpec> {
        self.specs.iter().find(|spec| spec.id.as_str() == id)
    }

    pub fn cli_operations(&self) -> Vec<&str> {
        self.specs
            .iter()
            .filter_map(|spec| match spec.adapters.cli {
                AdapterSupport::Available => Some(spec.id.as_str()),
                _ => None,
            })
            .collect()
    }
}

pub fn registry() -> CapabilityRegistry {
    CapabilityRegistry::new(vec![
        build_spec(),
        spec(
            "compile.build.watch",
            "Watch build",
            "Compile once, then rebuild when project sources change.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::WatchesUntilInterrupted,
            ),
            cli(),
        ),
        spec(
            DIFF_CAPABILITY_ID,
            "Diff builds",
            "Compare build artifacts produced from the current tree and another git ref.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text],
            read_only(),
            cli(),
        ),
        check_spec(),
        spec(
            "compile.convert",
            "Convert prompt",
            "Turn raw natural-language prompt text into a buildable prompt source.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(NetworkRequirement::Required, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "workflow.init",
            "Initialize project",
            "Scaffold a new Cybersin project layout.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(NetworkRequirement::None, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "compile.fmt",
            "Format prompt source",
            "Normalize one prompt source file's formatting.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text],
            writes_project_files(NetworkRequirement::None, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "compile.fmt.check",
            "Check prompt formatting",
            "Report whether one prompt source file is already canonically formatted.",
            CapabilityCategory::Compile,
            vec![OutputMode::Text],
            read_only(),
            cli(),
        ),
        spec(
            "runtime.run.stub",
            "Run stub agent",
            "Run the built-in stub agent against a compiled dist fixture.",
            CapabilityCategory::Runtime,
            vec![OutputMode::Text, OutputMode::Json],
            runtime_writes(
                ProcessLifecycle::None,
                LongRunningBehavior::StreamsUntilComplete,
                NetworkRequirement::None,
            ),
            cli(),
        ),
        spec(
            "runtime.run.agent",
            "Run agent",
            "Spawn an agent harness and drive a live runtime session.",
            CapabilityCategory::Runtime,
            vec![OutputMode::Text, OutputMode::Json],
            runtime_writes(
                ProcessLifecycle::StartsChildProcess,
                LongRunningBehavior::StreamsUntilComplete,
                NetworkRequirement::Required,
            ),
            cli(),
        ),
        spec(
            TRACE_LS_CAPABILITY_ID,
            "List traces",
            "List recorded spans from the trace store.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            generic_tui(),
        ),
        spec(
            TRACE_SHOW_CAPABILITY_ID,
            "Show trace",
            "Show one recorded span as JSON.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Json],
            runtime_read(),
            cli(),
        ),
        spec(
            "inspection.trace.sample",
            "Sample trace",
            "Promote one LLM span into a portable eval fixture.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text, OutputMode::Artifact],
            SafetyProfile {
                file_mutation: MutationLevel::WritesProjectFiles,
                ..runtime_read()
            },
            cli(),
        ),
        spec(
            COST_CAPABILITY_ID,
            "Roll up cost",
            "Group recorded cost by session, agent, model, tool, or day.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            "workflow.eval.run",
            "Run evals",
            "Execute eval suites and print score distributions.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text],
            SafetyProfile {
                process_lifecycle: ProcessLifecycle::StartsChildProcess,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::StreamsUntilComplete,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "workflow.eval.gate",
            "Gate evals",
            "Execute eval suites and fail if assertions regress.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text],
            SafetyProfile {
                process_lifecycle: ProcessLifecycle::StartsChildProcess,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::StreamsUntilComplete,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "inspection.explain",
            "Explain prompt",
            "Explain compiled prompt tokens, routing, costs, sessions, traces, and tools.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text, OutputMode::Tui],
            runtime_read(),
            cli(),
        ),
        spec(
            DOCTOR_CAPABILITY_ID,
            "Doctor setup",
            "Report local project setup readiness and focused next actions.",
            CapabilityCategory::Inspection,
            vec![OutputMode::Text],
            read_only(),
            cli(),
        ),
        spec(
            "workflow.setup",
            "Setup local readiness",
            "Create or update local OpenRouter-first config, then report setup readiness.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text],
            SafetyProfile {
                file_mutation: MutationLevel::WritesProjectFiles,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "control.ops",
            "Open ops",
            "Inspect and interact with project sessions, traces, costs, approvals, and builds.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Tui],
            SafetyProfile {
                runtime_state_mutation: MutationLevel::WritesRuntimeState,
                process_lifecycle: ProcessLifecycle::ControlsExistingProcess,
                network: NetworkRequirement::Optional,
                long_running: LongRunningBehavior::WatchesUntilInterrupted,
                confirmation: ConfirmationPolicy::Recommended,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            "control.daemon.server",
            "Run daemon server",
            "Run Postgres-backed TCP and mTLS multi-worker daemon mode.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            SafetyProfile {
                runtime_state_mutation: MutationLevel::WritesRuntimeState,
                process_lifecycle: ProcessLifecycle::StartsDaemon,
                network: NetworkRequirement::Required,
                long_running: LongRunningBehavior::Daemon,
                ..SafetyProfile::read_only()
            },
            cli(),
        ),
        spec(
            DLQ_LS_CAPABILITY_ID,
            "List dead letters",
            "List failed tool calls that have not been acknowledged.",
            CapabilityCategory::Control,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            DLQ_SHOW_CAPABILITY_ID,
            "Show dead letter",
            "Show one dead-lettered tool call as JSON.",
            CapabilityCategory::Control,
            vec![OutputMode::Json],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.dlq.retry",
            "Retry dead letter",
            "Reopen and rerun one dead-lettered tool call.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Json],
            tool_execution(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.dlq.drop",
            "Drop dead letter",
            "Acknowledge and hide one dead-lettered tool call without deleting its audit row.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.approve",
            "Approve parked call",
            "Resume an approval-gated parked tool call.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Json],
            tool_execution(ConfirmationPolicy::Required {
                reason: "runs a previously parked tool call".to_string(),
            }),
            cli(),
        ),
        spec(
            "control.deny",
            "Deny parked call",
            "Resolve a parked tool call as denied.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Required {
                reason: "changes a parked call outcome".to_string(),
            }),
            cli(),
        ),
        spec(
            SESSIONS_LS_CAPABILITY_ID,
            "List sessions",
            "List durable runtime sessions.",
            CapabilityCategory::Control,
            vec![OutputMode::Table, OutputMode::Text],
            runtime_read(),
            cli(),
        ),
        spec(
            SESSIONS_SHOW_CAPABILITY_ID,
            "Show session",
            "Show one session, its events, state, and latest checkpoint.",
            CapabilityCategory::Control,
            vec![OutputMode::Json],
            runtime_read(),
            cli(),
        ),
        spec(
            "control.sessions.resume",
            "Resume session",
            "Resume a durable session against a config hash.",
            CapabilityCategory::Control,
            vec![OutputMode::Text, OutputMode::Json],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.sessions.kill",
            "Kill session",
            "Mark a durable session as killed.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Required {
                reason: "stops a durable runtime session".to_string(),
            }),
            cli(),
        ),
        spec(
            "control.sessions.migrate",
            "Migrate session",
            "Move a durable session to a new config hash.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "control.sessions.materialize",
            "Materialize session artifacts",
            "Write a stored config artifact bundle back to a directory.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            writes_project_files(NetworkRequirement::None, LongRunningBehavior::Finite),
            cli(),
        ),
        spec(
            "control.notify",
            "Notify session",
            "Deliver a durable steering signal to a runtime session.",
            CapabilityCategory::Control,
            vec![OutputMode::Text],
            runtime_mutation(ConfirmationPolicy::Recommended),
            cli(),
        ),
        spec(
            "sandbox.exec",
            "Execute sandbox command",
            "Run a command in an isolated call or session workspace.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            sandbox_exec(),
            cli(),
        ),
        spec(
            "sandbox.snapshot",
            "Snapshot sandbox",
            "Snapshot a persistent session workspace checkpoint.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            sandbox_lifecycle(MutationLevel::WritesRuntimeState, ConfirmationPolicy::None),
            cli(),
        ),
        spec(
            SANDBOX_DIFF_CAPABILITY_ID,
            "Diff sandbox",
            "Show workspace changes relative to a checkpoint snapshot.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            SafetyProfile::read_only(),
            cli(),
        ),
        spec(
            "sandbox.restore",
            "Restore sandbox",
            "Restore a persistent session workspace to a checkpoint snapshot.",
            CapabilityCategory::Sandbox,
            vec![OutputMode::Text],
            sandbox_lifecycle(
                MutationLevel::Destructive,
                ConfirmationPolicy::Required {
                    reason: "replaces workspace files with checkpoint contents".to_string(),
                },
            ),
            cli(),
        ),
        spec(
            SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID,
            "Scaffold prompt agent",
            "Create the generated agent yaml and scripted harness for a prompt source.",
            CapabilityCategory::Compile,
            vec![OutputMode::Json, OutputMode::Artifact],
            writes_project_files(NetworkRequirement::None, LongRunningBehavior::Finite),
            unavailable("scaffold is invoked as a component of workflow.scaffold-build"),
        ),
        spec(
            "workflow.optimize",
            "Optimize project",
            "Analyze traces, emit an optimization report, and rebuild with observed routing stats.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::StreamsUntilComplete,
            ),
            cli(),
        ),
        workflow_spec(
            SCAFFOLD_BUILD_WORKFLOW_ID,
            "Scaffold and build prompt source",
            "Create the TUI's prompt-source scaffold and immediately build it.",
            CapabilityCategory::Workflow,
            vec![OutputMode::Text, OutputMode::Artifact],
            writes_project_files(
                NetworkRequirement::Optional,
                LongRunningBehavior::StreamsUntilComplete,
            ),
            custom_tui(),
            vec![
                CapabilityId::new(SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID),
                CapabilityId::new(BUILD_CAPABILITY_ID),
            ],
        ),
    ])
}

fn spec(
    id: &str,
    title: &str,
    summary: &str,
    category: CapabilityCategory,
    output_modes: Vec<OutputMode>,
    safety: SafetyProfile,
    adapters: AdapterCoverage,
) -> CapabilitySpec {
    CapabilitySpec {
        id: CapabilityId::new(id),
        title: title.to_string(),
        summary: summary.to_string(),
        category,
        components: Vec::new(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": true
        }),
        output_modes,
        safety,
        adapters,
    }
}

fn workflow_spec(
    id: &str,
    title: &str,
    summary: &str,
    category: CapabilityCategory,
    output_modes: Vec<OutputMode>,
    safety: SafetyProfile,
    adapters: AdapterCoverage,
    components: Vec<CapabilityId>,
) -> CapabilitySpec {
    let mut spec = spec(id, title, summary, category, output_modes, safety, adapters);
    spec.components = components;
    spec
}

fn build_spec() -> CapabilitySpec {
    CapabilitySpec {
        id: CapabilityId::new(BUILD_CAPABILITY_ID),
        title: "Build project".to_string(),
        summary: "Compile a project into dist artifacts.".to_string(),
        category: CapabilityCategory::Compile,
        components: Vec::new(),
        input_schema: json!({
            "type": "object",
            "required": ["project_path", "profile", "frozen"],
            "additionalProperties": false,
            "properties": {
                "project_path": {
                    "type": "string",
                    "description": "Project directory containing cybersin.yaml, cybersin.lock, and prompt sources."
                },
                "profile": {
                    "type": "string",
                    "enum": ["dev", "release"],
                    "description": "Build profile; dev excludes model-assisted compression."
                },
                "frozen": {
                    "type": "boolean",
                    "description": "Refuse build passes that would require network-backed updates."
                },
                "selected_prompt_source": {
                    "type": ["string", "null"],
                    "description": "Optional single *.prompt.yaml source to compile instead of all discovered prompts."
                }
            }
        }),
        output_modes: vec![OutputMode::Text, OutputMode::Json, OutputMode::Artifact],
        safety: writes_project_files(
            NetworkRequirement::Optional,
            LongRunningBehavior::StreamsUntilComplete,
        ),
        adapters: cli(),
    }
}

fn check_spec() -> CapabilitySpec {
    CapabilitySpec {
        id: CapabilityId::new(CHECK_CAPABILITY_ID),
        title: "Check prompt sources".to_string(),
        summary: "Parse, include-resolve, typecheck, and emit prompt IR.".to_string(),
        category: CapabilityCategory::Compile,
        components: Vec::new(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Prompt source file, project directory, or prompts directory to check."
                }
            }
        }),
        output_modes: vec![OutputMode::Text, OutputMode::Json],
        safety: read_only(),
        adapters: generic_tui(),
    }
}

fn cli() -> AdapterCoverage {
    AdapterCoverage {
        cli: AdapterSupport::Available,
        tui: AdapterSupport::Unavailable {
            reason: "capability invocation is not wired into the TUI adapter yet".to_string(),
        },
    }
}

fn generic_tui() -> AdapterCoverage {
    AdapterCoverage {
        cli: AdapterSupport::Available,
        tui: AdapterSupport::Generic,
    }
}

fn custom_tui() -> AdapterCoverage {
    AdapterCoverage {
        cli: AdapterSupport::Unavailable {
            reason: "TUI-only workflow".to_string(),
        },
        tui: AdapterSupport::Custom,
    }
}

fn unavailable(reason: &str) -> AdapterCoverage {
    AdapterCoverage {
        cli: AdapterSupport::Unavailable {
            reason: reason.to_string(),
        },
        tui: AdapterSupport::Unavailable {
            reason: reason.to_string(),
        },
    }
}

fn read_only() -> SafetyProfile {
    SafetyProfile::read_only()
}

fn runtime_read() -> SafetyProfile {
    SafetyProfile {
        process_lifecycle: ProcessLifecycle::ControlsExistingProcess,
        ..SafetyProfile::read_only()
    }
}

fn runtime_writes(
    process_lifecycle: ProcessLifecycle,
    long_running: LongRunningBehavior,
    network: NetworkRequirement,
) -> SafetyProfile {
    SafetyProfile {
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle,
        network,
        long_running,
        ..SafetyProfile::read_only()
    }
}

fn runtime_mutation(confirmation: ConfirmationPolicy) -> SafetyProfile {
    SafetyProfile {
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle: ProcessLifecycle::ControlsExistingProcess,
        confirmation,
        ..SafetyProfile::read_only()
    }
}

fn writes_project_files(
    network: NetworkRequirement,
    long_running: LongRunningBehavior,
) -> SafetyProfile {
    SafetyProfile {
        file_mutation: MutationLevel::WritesProjectFiles,
        network,
        long_running,
        ..SafetyProfile::read_only()
    }
}

fn tool_execution(confirmation: ConfirmationPolicy) -> SafetyProfile {
    SafetyProfile {
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle: ProcessLifecycle::StartsChildProcess,
        network: NetworkRequirement::Optional,
        long_running: LongRunningBehavior::StreamsUntilComplete,
        confirmation,
        ..SafetyProfile::read_only()
    }
}

fn sandbox_exec() -> SafetyProfile {
    SafetyProfile {
        file_mutation: MutationLevel::WritesRuntimeState,
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        process_lifecycle: ProcessLifecycle::StartsChildProcess,
        long_running: LongRunningBehavior::StreamsUntilComplete,
        confirmation: ConfirmationPolicy::Recommended,
        ..SafetyProfile::read_only()
    }
}

fn sandbox_lifecycle(
    file_mutation: MutationLevel,
    confirmation: ConfirmationPolicy,
) -> SafetyProfile {
    SafetyProfile {
        file_mutation,
        runtime_state_mutation: MutationLevel::WritesRuntimeState,
        confirmation,
        ..SafetyProfile::read_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use cybersin_gateway::{EchoExecutor, ToolGateway};
    use cybersin_runtime::{SqliteStorage, Storage};
    use cybersin_sandbox::{SandboxScope, WorkspaceStore};
    use cybersin_trace::{CacheStatus, SpanKind, SpanStatus};
    use serde_json::json;

    #[test]
    fn constructs_capability_spec_metadata() {
        let spec = CapabilitySpec {
            id: CapabilityId::new("compile.check"),
            title: "Check prompt sources".to_string(),
            summary: "Parse, include-resolve, and typecheck prompt sources.".to_string(),
            category: CapabilityCategory::Compile,
            components: Vec::new(),
            input_schema: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" }
                }
            }),
            output_modes: vec![OutputMode::Text, OutputMode::Json],
            safety: SafetyProfile::read_only(),
            adapters: AdapterCoverage {
                cli: AdapterSupport::Available,
                tui: AdapterSupport::Generic,
            },
        };

        assert_eq!(spec.id.as_str(), "compile.check");
        assert_eq!(spec.category, CapabilityCategory::Compile);
        assert_eq!(spec.input_schema["required"][0], "path");
        assert_eq!(spec.output_modes, vec![OutputMode::Text, OutputMode::Json]);
        assert_eq!(spec.safety.file_mutation, MutationLevel::None);
        assert_eq!(spec.adapters.tui, AdapterSupport::Generic);
    }

    #[test]
    fn safety_profile_covers_shared_policy_axes() {
        let safety = SafetyProfile {
            file_mutation: MutationLevel::Destructive,
            runtime_state_mutation: MutationLevel::WritesRuntimeState,
            process_lifecycle: ProcessLifecycle::StartsDaemon,
            network: NetworkRequirement::Required,
            long_running: LongRunningBehavior::Daemon,
            confirmation: ConfirmationPolicy::Required {
                reason: "drops durable runtime state".to_string(),
            },
        };

        assert_eq!(safety.file_mutation, MutationLevel::Destructive);
        assert_eq!(
            safety.runtime_state_mutation,
            MutationLevel::WritesRuntimeState
        );
        assert_eq!(safety.process_lifecycle, ProcessLifecycle::StartsDaemon);
        assert_eq!(safety.network, NetworkRequirement::Required);
        assert_eq!(safety.long_running, LongRunningBehavior::Daemon);
        assert_eq!(
            safety.confirmation,
            ConfirmationPolicy::Required {
                reason: "drops durable runtime state".to_string()
            }
        );
    }

    #[test]
    fn constructs_execution_events() {
        let capability_id = CapabilityId::new("runtime.trace.ls");
        let events = vec![
            CapabilityEvent::Started {
                capability_id: capability_id.clone(),
            },
            CapabilityEvent::Progress {
                message: "loading spans".to_string(),
                current: Some(1),
                total: Some(2),
            },
            CapabilityEvent::Prompt {
                id: "confirm-drop".to_string(),
                message: "Drop selected rows?".to_string(),
                confirmation: ConfirmationPolicy::Required {
                    reason: "destructive action".to_string(),
                },
            },
            CapabilityEvent::Output {
                mode: OutputMode::Table,
                value: json!([{ "session": "abc", "spans": 3 }]),
            },
            CapabilityEvent::Completed {
                value: Some(json!({ "rows": 1 })),
            },
            CapabilityEvent::Failed {
                message: "storage unavailable".to_string(),
            },
        ];

        assert!(matches!(
            events[0],
            CapabilityEvent::Started {
                ref capability_id
            } if capability_id.as_str() == "runtime.trace.ls"
        ));
        assert!(matches!(
            events[1],
            CapabilityEvent::Progress {
                current: Some(1),
                total: Some(2),
                ..
            }
        ));
        assert!(matches!(
            events[2],
            CapabilityEvent::Prompt {
                confirmation: ConfirmationPolicy::Required { .. },
                ..
            }
        ));
        assert!(matches!(
            events[3],
            CapabilityEvent::Output {
                mode: OutputMode::Table,
                ..
            }
        ));
        assert!(matches!(events[4], CapabilityEvent::Completed { .. }));
        assert!(matches!(events[5], CapabilityEvent::Failed { .. }));
    }

    #[test]
    fn registry_contains_current_catalog_with_unique_ids() {
        use std::collections::BTreeSet;

        let registry = registry();
        let mut ids = BTreeSet::new();
        for spec in registry.specs() {
            assert!(
                ids.insert(spec.id.as_str()),
                "duplicate capability id {}",
                spec.id.as_str()
            );
            assert!(
                spec.input_schema.is_object(),
                "{} should declare a structured input schema",
                spec.id.as_str()
            );
            assert!(
                !spec.output_modes.is_empty(),
                "{} should declare at least one output mode",
                spec.id.as_str()
            );
            match &spec.adapters.tui {
                AdapterSupport::Generic
                    if matches!(
                        spec.id.as_str(),
                        CHECK_CAPABILITY_ID | TRACE_LS_CAPABILITY_ID
                    ) => {}
                AdapterSupport::Custom if spec.id.as_str() == SCAFFOLD_BUILD_WORKFLOW_ID => {}
                AdapterSupport::Unavailable { reason } => assert!(
                    !reason.is_empty(),
                    "{} should explain why TUI capability invocation is unavailable",
                    spec.id.as_str()
                ),
                other => panic!(
                    "{} unexpectedly declares TUI adapter support: {other:?}",
                    spec.id.as_str()
                ),
            }
        }

        assert!(registry.get("compile.build").is_some());
        assert_eq!(
            registry
                .get(CHECK_CAPABILITY_ID)
                .map(|spec| &spec.adapters.tui),
            Some(&AdapterSupport::Generic)
        );
        assert_eq!(
            registry
                .get(TRACE_LS_CAPABILITY_ID)
                .map(|spec| &spec.adapters.tui),
            Some(&AdapterSupport::Generic)
        );
        assert!(registry.get("runtime.run.stub").is_some());
        assert!(registry.get("runtime.run.agent").is_some());
        assert!(registry.get("sandbox.restore").is_some());
        let scaffold_build = registry.get(SCAFFOLD_BUILD_WORKFLOW_ID).unwrap();
        assert_eq!(
            scaffold_build.components,
            vec![
                CapabilityId::new(SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID),
                CapabilityId::new(BUILD_CAPABILITY_ID),
            ]
        );
        assert_eq!(scaffold_build.adapters.tui, AdapterSupport::Custom);
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn scaffold_build_workflow_invokes_components_in_order_and_hands_off_typed_output() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let source = project.path().join("prompts/hello.prompt.yaml");
        std::fs::write(
            &source,
            "name: hello\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Hello.\n",
        )
        .unwrap();

        let execution = execute_scaffold_build(ScaffoldBuildInput::new(
            project.path(),
            &source,
            BuildProfile::Dev,
            false,
        ));

        assert!(execution.is_success());
        let started = execution
            .events
            .iter()
            .filter_map(|event| match event {
                CapabilityEvent::Started { capability_id } => Some(capability_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            started,
            vec![
                SCAFFOLD_BUILD_WORKFLOW_ID,
                SCAFFOLD_PROMPT_AGENT_CAPABILITY_ID,
                BUILD_CAPABILITY_ID,
            ]
        );

        let workflow_value = execution.events.iter().rev().find_map(|event| match event {
            CapabilityEvent::Completed { value: Some(value) } => {
                value.get("scaffold").map(|_| value)
            }
            _ => None,
        });
        let value = workflow_value.expect("workflow completion value");
        assert_eq!(
            value["scaffold"]["agent_path"].as_str().unwrap(),
            project
                .path()
                .join("agents/hello.agent.yaml")
                .display()
                .to_string()
        );
        assert!(project.path().join("agents/hello.agent.yaml").exists());
        assert!(project.path().join("harnesses/hello.script.yaml").exists());
        assert!(project.path().join("dist/prompts/hello.json").exists());
    }

    #[test]
    fn doctor_cli_renderer_matches_direct_capability_text() {
        let tmp = tempfile::tempdir().unwrap();
        let direct = execute_doctor(DoctorInput::new(tmp.path()));
        let capability_text = direct_text_output(&direct.events);

        assert_eq!(rendered_text(&direct.events), capability_text);
        assert!(capability_text.contains("Cybersin doctor"));
        assert_eq!(
            simple_result(&direct.events),
            Some(Err(
                "project is not ready; see next actions above".to_string()
            ))
        );
    }

    #[tokio::test]
    async fn cost_cli_renderer_matches_direct_capability_text() {
        let spans = SpanStore::in_memory().await.unwrap();
        spans.insert(&sample_span("span-1", 1)).await.unwrap();

        let direct = execute_cost(
            &spans,
            CostInput {
                by: CostDimension::Model,
            },
        )
        .await;
        let capability_text = direct_text_output(&direct.events);

        assert_eq!(rendered_text(&direct.events), capability_text);
        assert!(capability_text.contains("MODEL"));
        assert!(capability_text.contains("gpt-4o-mini"));
        assert_eq!(simple_result(&direct.events), Some(Ok(())));
    }

    #[tokio::test]
    async fn trace_show_cli_renderer_matches_direct_capability_text() {
        let spans = SpanStore::in_memory().await.unwrap();
        spans.insert(&sample_span("span-1", 1)).await.unwrap();

        let direct = execute_trace_show(
            &spans,
            TraceShowInput {
                id: "span-1".to_string(),
            },
        )
        .await;
        let capability_text = direct_text_output(&direct.events);

        assert_eq!(rendered_text(&direct.events), capability_text);
        assert!(capability_text.contains("\"id\": \"span-1\""));
        assert_eq!(simple_result(&direct.events), Some(Ok(())));
    }

    #[tokio::test]
    async fn sessions_cli_renderer_matches_direct_capability_text() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage
            .create_session_pinned("sess-1", "agent-a", "hash-a")
            .await
            .unwrap();
        storage
            .append_event("sess-1", "session.started", json!({"input": 1}))
            .await
            .unwrap();
        storage
            .set_state("sess-1", "vars", "answer", &json!(42))
            .await
            .unwrap();

        let ls = execute_sessions_ls(&storage, SessionsLsInput { json: false }).await;
        let show = execute_sessions_show(
            &storage,
            SessionsShowInput {
                session: "sess-1".to_string(),
            },
        )
        .await;

        assert_eq!(rendered_text(&ls.events), direct_text_output(&ls.events));
        assert!(rendered_text(&ls.events).contains("sess-1"));
        assert_eq!(simple_result(&ls.events), Some(Ok(())));
        assert_eq!(
            rendered_text(&show.events),
            direct_text_output(&show.events)
        );
        assert!(rendered_text(&show.events).contains("\"session_id\": \"sess-1\""));
        assert_eq!(simple_result(&show.events), Some(Ok(())));
    }

    #[tokio::test]
    async fn dlq_cli_renderer_matches_direct_capability_text() {
        let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
        storage
            .create_session_pinned("sess-1", "agent-a", "hash-a")
            .await
            .unwrap();
        storage
            .begin_tool_call(
                "tool:key",
                "sess-1",
                "tool",
                "key",
                "write",
                &json!({"x": 1}),
            )
            .await
            .unwrap();
        storage
            .resolve_tool_call_failed("tool:key", "boom", true)
            .await
            .unwrap();
        let gateway = ToolGateway::new(storage, Arc::new(EchoExecutor));

        let ls = execute_dlq_ls(&gateway).await;
        let show = execute_dlq_show(
            &gateway,
            DlqShowInput {
                call_id: "tool:key".to_string(),
            },
        )
        .await;

        assert_eq!(rendered_text(&ls.events), direct_text_output(&ls.events));
        assert!(rendered_text(&ls.events).contains("tool:key"));
        assert_eq!(simple_result(&ls.events), Some(Ok(())));
        assert_eq!(
            rendered_text(&show.events),
            direct_text_output(&show.events)
        );
        assert!(rendered_text(&show.events).contains("\"failure_reason\": \"boom\""));
        assert_eq!(simple_result(&show.events), Some(Ok(())));
    }

    #[test]
    fn sandbox_diff_cli_operation_output_matches_direct_capability_text() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sandbox");
        let store = WorkspaceStore::new(&root).unwrap();
        let workspace = store.open(SandboxScope::Session, "sess-1", "cli").unwrap();
        std::fs::write(workspace.path().join("state.txt"), "before").unwrap();
        workspace.snapshot("checkpoint-1").unwrap();
        std::fs::write(workspace.path().join("state.txt"), "after").unwrap();
        std::fs::write(workspace.path().join("new.txt"), "created").unwrap();

        let input = SandboxDiffInput {
            root,
            session: "sess-1".to_string(),
            checkpoint: "checkpoint-1".to_string(),
        };
        let expected = sandbox::diff_text(&sandbox::LifecycleArgs {
            root: input.root.clone(),
            session: input.session.clone(),
            checkpoint: input.checkpoint.clone(),
        })
        .unwrap();
        let direct = execute_sandbox_diff(input);

        assert_eq!(rendered_text(&direct.events), expected);
        assert!(expected.contains("A new.txt"));
        assert!(expected.contains("M state.txt"));
        assert_eq!(simple_result(&direct.events), Some(Ok(())));
    }

    #[test]
    fn diff_summary_adapter_matches_direct_capability_result() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo_with_project(&repo);

        let output = crate::commands::diff::run_with_output(
            &repo.join("project"),
            "HEAD",
            BuildProfile::Dev,
        )
        .unwrap();
        let direct = execute_diff(DiffInput {
            project_path: repo.join("project"),
            reference: "HEAD".to_string(),
            profile: BuildProfile::Dev,
        });

        assert_eq!(rendered_text(&direct.events), output.text);
        assert_eq!(
            crate::commands::diff::diff_summary(&direct.events),
            Some(Ok(Some(output.summary)))
        );
    }

    fn direct_text_output(events: &[CapabilityEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                CapabilityEvent::Output {
                    mode: OutputMode::Text,
                    value,
                } => output_stream(value).map(|(_, text)| text.to_string()),
                _ => None,
            })
            .collect()
    }

    fn sample_span(id: &str, start_unix_ms: i64) -> Span {
        Span {
            id: id.to_string(),
            session_id: "sess-1".to_string(),
            agent_name: "agent-a".to_string(),
            kind: SpanKind::LlmCall,
            name: "researcher".to_string(),
            start_unix_ms,
            end_unix_ms: start_unix_ms + 10,
            model: Some("gpt-4o-mini".to_string()),
            tokens_prompt: Some(120),
            tokens_completion: Some(40),
            usd_cost: 0.000432,
            cache_status: CacheStatus::Miss,
            retries: 0,
            evicted_sections: vec![],
            status: SpanStatus::Ok,
            attributes: json!({}),
        }
    }

    fn init_repo_with_project(repo: &Path) {
        use std::process::Command;

        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "test@example.com"]);
        git(repo, &["config", "user.name", "Test"]);
        let project = repo.join("project");
        crate::commands::init::run(&project).expect("init");
        std::fs::write(
            project.join("prompts/hello.prompt.yaml"),
            r#"name: hello
quality: medium
inputs:
  name: string
sections:
  - id: instructions
    priority: 100
    body: "Greet {{ name }}."
"#,
        )
        .unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "initial"]);

        fn git(dir: &Path, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }
    }
}
