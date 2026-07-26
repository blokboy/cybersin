//! `cybersin ops [path]`: live-refreshing Sessions/Traces/Cost/Approvals
//! control room (issues #51, #52). Follows `explain`'s "one view model,
//! two renderers" shape (`crates/cybersin-cli/src/commands/explain.rs`)
//! for the interactive TUI + `--plain`/non-TTY text fallback, and reuses
//! its Sessions/Traces/Cost query patterns verbatim. Two things differ
//! from `explain`: there's no compiled prompt to explain (`ops` is
//! project-scoped, not prompt-scoped), and the model must stay live —
//! `explain`'s terminal loop is synchronous and loads its model once, so
//! `ops` instead refreshes a shared model from a background task on an
//! interval while the (still-synchronous, crossterm-driven) terminal
//! loop keeps redrawing from whatever that task last wrote.
//!
//! `explain`'s own `path` positional argument is never run through
//! issue #50's project-root discovery — it's resolved as a plain
//! CWD-relative default, same as before #50 existed. `ops`'s `path`
//! *is* run through that discovery (see [`OpsArgs::path`]), since the
//! issue calls for `cybersin ops` to need no explicit flags to see the
//! current project's live data.
//!
//! # Why the Approvals tab's `ToolGateway` is built lazily
//!
//! Sessions/Traces/Cost are read-only against `Storage`/`SpanStore` and
//! never needed `--dist`. Approving/denying a parked call, though, goes
//! through the exact same in-process `ToolGateway` machinery `cybersin
//! approve`/`deny` use (`crates/cybersin-gateway/src/gateway.rs`), which
//! needs a real compiled `dist/` to build an executor from
//! (`configured_executor`) — and a freshly-`cybersin init`ed project's
//! `dist/` is an empty directory until `cybersin build` runs. Resolving
//! that eagerly at `ops` startup would make plain viewing fail on a
//! project that has never been built. Instead [`GatewayContext::build`]
//! is only ever called from inside the action-handling task, the moment
//! a human actually presses `a`/`d` — so browsing tabs never pays that
//! cost or that requirement.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use cybersin_gateway::{GatewayOutcome, ToolGateway};
use cybersin_runtime::{DaemonHandle, SessionRecord, ToolCallRecord};
use cybersin_trace::{CostDimension, CostRollupRow, Span, SpanFilter};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::Terminal;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::commands::build::discover_agent_sources;
use crate::commands::run::{self, RunArgs};
use crate::commands::sandbox::Backend;
use crate::harness_config::AgentMeta;
use crate::project::{self, ProjectDefaults};
use crate::tool_executor::configured_executor;

/// How often the TUI's Sessions/Traces/Cost/Approvals tabs re-query the
/// daemon.
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Args)]
pub struct OpsArgs {
    /// Project directory (or a subdirectory of one). Defaults to the
    /// current directory. The project root `--db` resolves against is
    /// discovered by walking up from here for a `cybersin.yaml` (issue
    /// #50) — so `cybersin ops`, with no other flags, shows live data
    /// for whatever project you're standing inside of.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Print a stable text snapshot instead of opening the interactive TUI.
    #[arg(long)]
    pub plain: bool,
}

pub(crate) struct PlainOpsSections {
    pub builds: String,
    pub sessions: String,
    pub traces: String,
    pub cost: String,
    pub approvals: String,
}

pub(crate) async fn plain_sections_for_path(path: &Path) -> Result<PlainOpsSections> {
    let start = std::fs::canonicalize(path)
        .with_context(|| format!("resolving project path {}", path.display()))?;
    let defaults = ProjectDefaults::detect(&start)?;
    let project_root = project::discover_project_root(&start).unwrap_or(start.clone());
    let daemon = DaemonHandle::auto_start(defaults.db_default()).await?;
    let model = OpsModel::load(&daemon, &project_root).await?;
    Ok(PlainOpsSections {
        builds: model.builds_text(),
        sessions: model.sessions_text(),
        traces: model.traces_text(),
        cost: model.costs_text(),
        approvals: model.approvals_text(),
    })
}

/// Everything needed to build a [`ToolGateway`] on demand — see this
/// module's doc for why it isn't built eagerly.
struct GatewayContext {
    dist: Option<PathBuf>,
    sandbox_root: Option<PathBuf>,
    sandbox_backend: Option<Backend>,
    defaults: ProjectDefaults,
}

#[derive(Clone)]
struct RunContext {
    project_root: PathBuf,
    db: PathBuf,
    dist: PathBuf,
    sandbox_root: PathBuf,
    sandbox_backend: Backend,
}

impl GatewayContext {
    fn build(&self, daemon: &DaemonHandle) -> Result<ToolGateway> {
        let dist_dir = match &self.dist {
            Some(dist) => dist.clone(),
            None => self.defaults.dist_default()?,
        };
        let sandbox_root = self
            .sandbox_root
            .clone()
            .unwrap_or_else(|| self.defaults.sandbox_root_default());
        let sandbox_backend = self
            .sandbox_backend
            .unwrap_or_else(|| self.defaults.sandbox_backend_default());
        let executor = configured_executor(&dist_dir, &sandbox_root, sandbox_backend)?;
        Ok(ToolGateway::new(daemon.storage(), executor))
    }
}

#[derive(Debug, Clone, Default)]
struct OpsModel {
    builds: Vec<OpsBuild>,
    sessions: Vec<SessionRecord>,
    spans: Vec<Span>,
    costs: Vec<CostRollupRow>,
    approvals: Vec<ToolCallRecord>,
}

impl OpsModel {
    async fn load(daemon: &DaemonHandle, project_root: &Path) -> Result<Self> {
        let spans = daemon
            .spans()
            .list(&SpanFilter {
                limit: Some(1_000),
                ..SpanFilter::default()
            })
            .await?;
        Ok(Self {
            builds: discover_ops_builds(project_root)?,
            sessions: daemon.storage().list_sessions().await?,
            costs: daemon.spans().cost_rollup(CostDimension::Model).await?,
            spans: spans.into_iter().take(25).collect(),
            approvals: daemon.storage().list_awaiting_approval().await?,
        })
    }

    fn builds_text(&self) -> String {
        let mut out = format!("Builds ({})\n", self.builds.len());
        if self.builds.is_empty() {
            out.push_str("  no agents found in agents/**/*.agent.yaml\n");
        }
        for build in &self.builds {
            out.push_str(&format!(
                "  {:<24} {:<8} {}\n",
                build.name,
                build.build_hash_short.as_deref().unwrap_or("unbuilt"),
                build.path.display()
            ));
        }
        out
    }

    fn sessions_text(&self) -> String {
        let mut out = format!("Sessions ({})\n", self.sessions.len());
        if self.sessions.is_empty() {
            out.push_str("  no sessions recorded\n");
        }
        for session in &self.sessions {
            out.push_str(&format!(
                "  {}  {}  {}  {}\n",
                session.session_id, session.status, session.agent_name, session.config_hash
            ));
        }
        out
    }

    fn traces_text(&self) -> String {
        let mut out = format!("Recent traces ({})\n", self.spans.len());
        if self.spans.is_empty() {
            out.push_str("  no spans recorded yet\n");
        }
        for span in &self.spans {
            out.push_str(&format!(
                "  {}  {:<14} {:<18} {:<16} ${:.6}\n",
                span.id,
                span.kind.as_str(),
                span.name,
                span.model.as_deref().unwrap_or("-"),
                span.usd_cost
            ));
        }
        out
    }

    fn costs_text(&self) -> String {
        let mut out = String::from("Cost by model\n");
        if self.costs.is_empty() {
            out.push_str("  no cost data recorded\n");
        }
        for row in &self.costs {
            out.push_str(&format!(
                "  {:<24} ${:.6}  {} spans  {} prompt / {} completion tokens\n",
                row.key, row.usd_cost, row.span_count, row.tokens_prompt, row.tokens_completion
            ));
        }
        out
    }

    /// One line per parked call — shared by the `--plain` report and the
    /// interactive Approvals tab's list rows.
    fn approval_lines(&self) -> Vec<String> {
        self.approvals
            .iter()
            .map(|row| {
                format!(
                    "{:<28} {:<14} {:<12} parked {}",
                    row.call_id,
                    row.tool,
                    row.session_id,
                    parked_for(row.updated_unix_ms)
                )
            })
            .collect()
    }

    fn approvals_text(&self) -> String {
        let mut out = format!("Approvals ({})\n", self.approvals.len());
        if self.approvals.is_empty() {
            out.push_str("  no calls awaiting approval\n");
        }
        for line in self.approval_lines() {
            out.push_str(&format!("  {line}\n"));
        }
        out
    }

    fn plain_report(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}",
            self.builds_text(),
            self.sessions_text(),
            self.traces_text(),
            self.costs_text(),
            self.approvals_text()
        )
    }
}

#[derive(Debug, Clone, Default)]
struct OpsBuild {
    name: String,
    path: PathBuf,
    build_hash_short: Option<String>,
}

fn discover_ops_builds(project_root: &Path) -> Result<Vec<OpsBuild>> {
    let build_hash_short = read_dist_build_hash(project_root).ok().map(short_hash);
    let mut builds = Vec::new();
    for path in discover_agent_sources(project_root).map_err(anyhow::Error::msg)? {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let meta = AgentMeta::from_agent_yaml(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        builds.push(OpsBuild {
            name: meta.name,
            path,
            build_hash_short: build_hash_short.clone(),
        });
    }
    builds.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    Ok(builds)
}

fn read_dist_build_hash(project_root: &Path) -> Result<String> {
    let manifest_path = project_root.join("dist/manifest.json");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    Ok(manifest
        .get("build_hash")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unbuilt")
        .to_string())
}

fn short_hash(hash: String) -> String {
    hash.get(..12).unwrap_or(&hash).to_string()
}

/// How long ago (relative to now) `updated_unix_ms` was — the Approvals
/// tab's "how long it's been parked" column (issue #52's acceptance
/// criteria).
fn parked_for(updated_unix_ms: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let secs = (now_ms - updated_unix_ms).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Mirrors `commands::dlq::print_outcome`, but as a string for the
/// Approvals tab's status line instead of stdout.
fn describe_outcome(call_id: &str, outcome: &GatewayOutcome) -> String {
    match outcome {
        GatewayOutcome::Resolved(cybersin_adapter::messages::CallOutcome::Ok { value }) => {
            format!("{call_id} succeeded: {value}")
        }
        GatewayOutcome::Resolved(cybersin_adapter::messages::CallOutcome::Failed {
            reason,
            retriable,
        }) => {
            format!("{call_id} failed (retriable={retriable}): {reason}")
        }
        GatewayOutcome::Parked { approval_id, .. } => {
            format!("{call_id} parked, awaiting approval {approval_id}")
        }
    }
}

pub async fn execute(
    db: Option<PathBuf>,
    dist: Option<PathBuf>,
    sandbox_root: Option<PathBuf>,
    sandbox_backend: Option<Backend>,
    args: OpsArgs,
) -> Result<()> {
    let start = std::fs::canonicalize(&args.path)
        .with_context(|| format!("resolving project path {}", args.path.display()))?;
    let defaults = ProjectDefaults::detect(&start)?;
    let project_root = project::discover_project_root(&start).unwrap_or(start.clone());
    let db = db.unwrap_or_else(|| defaults.db_default());

    let daemon = DaemonHandle::auto_start(db.clone()).await?;
    let model = OpsModel::load(&daemon, &project_root).await?;

    if args.plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!("{}", model.plain_report());
        return Ok(());
    }

    let gateway_ctx = GatewayContext {
        dist,
        sandbox_root,
        sandbox_backend,
        defaults,
    };
    let run_dist = gateway_ctx
        .dist
        .clone()
        .unwrap_or(gateway_ctx.defaults.dist_default()?);
    let run_sandbox_root = gateway_ctx
        .sandbox_root
        .clone()
        .unwrap_or_else(|| gateway_ctx.defaults.sandbox_root_default());
    let run_sandbox_backend = gateway_ctx
        .sandbox_backend
        .unwrap_or_else(|| gateway_ctx.defaults.sandbox_backend_default());
    let run_ctx = RunContext {
        project_root,
        db,
        dist: run_dist,
        sandbox_root: run_sandbox_root,
        sandbox_backend: run_sandbox_backend,
    };
    run_tui(daemon, model, gateway_ctx, run_ctx).await
}

/// One in-place approve/deny request from the Approvals tab.
enum GatewayAction {
    Approve(String),
    Deny(String),
}

enum RunAction {
    Run(PathBuf),
}

/// Runs the interactive TUI. A background task refreshes `shared` from
/// the daemon every [`REFRESH_INTERVAL`]; the terminal loop itself stays
/// synchronous (crossterm's input model isn't async here) and simply
/// redraws from whatever `shared` currently holds on every iteration —
/// which happens at least every 250ms regardless of key input — so a
/// refresh lands on screen without the user ever leaving `ops`. A second
/// background task owns the (lazily built, see this module's doc)
/// `ToolGateway` and resolves `GatewayAction`s sent from the terminal
/// loop over an unbounded channel — `UnboundedSender::send` is a
/// synchronous, non-blocking call, so the sync terminal loop can use it
/// directly without needing its own async runtime.
async fn run_tui(
    daemon: DaemonHandle,
    initial: OpsModel,
    gateway_ctx: GatewayContext,
    run_ctx: RunContext,
) -> Result<()> {
    let shared = Arc::new(Mutex::new(initial));
    let status: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let refresh_shared = Arc::clone(&shared);
    let refresh_daemon = daemon.clone();
    let refresh_project_root = run_ctx.project_root.clone();
    let refresh_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
        ticker.tick().await; // first tick fires immediately; the initial load already covers it
        loop {
            ticker.tick().await;
            if let Ok(model) = OpsModel::load(&refresh_daemon, &refresh_project_root).await {
                *refresh_shared.lock().unwrap() = model;
            }
        }
    });

    let (action_tx, action_rx) = unbounded_channel::<GatewayAction>();
    let action_shared = Arc::clone(&shared);
    let action_status = Arc::clone(&status);
    let action_daemon = daemon.clone();
    let action_task = tokio::spawn(run_gateway_actions(
        action_daemon,
        gateway_ctx,
        run_ctx.project_root.clone(),
        action_rx,
        action_shared,
        action_status,
    ));

    let (run_tx, run_rx) = unbounded_channel::<RunAction>();
    let run_shared = Arc::clone(&shared);
    let run_status = Arc::clone(&status);
    let run_daemon = daemon.clone();
    let run_project_root = run_ctx.project_root.clone();
    let run_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(run_agents(
                run_ctx,
                run_rx,
                run_daemon,
                run_shared,
                run_status,
                run_project_root,
            )),
            Err(error) => {
                *run_status.lock().unwrap() =
                    Some(format!("error: failed to start run worker: {error}"));
            }
        }
    });

    let ui_shared = Arc::clone(&shared);
    let ui_status = Arc::clone(&status);
    let ui_result = tokio::task::spawn_blocking(move || {
        run_terminal_loop(&ui_shared, &ui_status, action_tx, run_tx)
    })
    .await;
    refresh_task.abort();
    action_task.abort();
    drop(run_thread);

    ui_result.context("ops terminal task panicked")?
}

/// Consumes `GatewayAction`s one at a time: builds a fresh `ToolGateway`
/// (see this module's doc for why it's rebuilt per action rather than
/// cached), resolves the call, records a status line, then forces an
/// immediate model reload so the resolved row disappears from the
/// Approvals tab without waiting for the next periodic tick.
async fn run_gateway_actions(
    daemon: DaemonHandle,
    gateway_ctx: GatewayContext,
    project_root: PathBuf,
    mut action_rx: UnboundedReceiver<GatewayAction>,
    shared: Arc<Mutex<OpsModel>>,
    status: Arc<Mutex<Option<String>>>,
) {
    while let Some(action) = action_rx.recv().await {
        let call_id = match &action {
            GatewayAction::Approve(id) | GatewayAction::Deny(id) => id.clone(),
        };
        let message = match gateway_ctx.build(&daemon) {
            Ok(gateway) => {
                let outcome = match &action {
                    GatewayAction::Approve(id) => gateway.approve(id).await,
                    GatewayAction::Deny(id) => gateway.deny(id).await,
                };
                match outcome {
                    Ok(outcome) => describe_outcome(&call_id, &outcome),
                    Err(error) => format!("{call_id}: error: {error}"),
                }
            }
            Err(error) => format!("{call_id}: error: {error}"),
        };
        *status.lock().unwrap() = Some(message);
        if let Ok(model) = OpsModel::load(&daemon, &project_root).await {
            *shared.lock().unwrap() = model;
        }
    }
}

async fn run_agents(
    run_ctx: RunContext,
    mut run_rx: UnboundedReceiver<RunAction>,
    daemon: DaemonHandle,
    shared: Arc<Mutex<OpsModel>>,
    status: Arc<Mutex<Option<String>>>,
    project_root: PathBuf,
) {
    while let Some(action) = run_rx.recv().await {
        let RunAction::Run(agent_yaml) = action;
        let display_name = agent_yaml
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("agent")
            .to_string();
        *status.lock().unwrap() = Some(format!("running {display_name}..."));
        let result = run::run_session(
            run_ctx.db.clone(),
            run_ctx.dist.clone(),
            run_ctx.sandbox_root.clone(),
            run_ctx.sandbox_backend,
            RunArgs {
                agent_yaml: Some(agent_yaml.clone()),
                stub: false,
                session_id: None,
                agent: None,
                input: None,
            },
        )
        .await;
        *status.lock().unwrap() = Some(match result {
            Ok(summary) => format!(
                "{} {}: {} span(s)",
                summary.session_id,
                if summary.completed {
                    "completed"
                } else {
                    "aborted"
                },
                summary.spans_recorded
            ),
            Err(error) => format!("{} failed: {error}", agent_yaml.display()),
        });
        if let Ok(model) = OpsModel::load(&daemon, &project_root).await {
            *shared.lock().unwrap() = model;
        }
    }
}

fn run_terminal_loop(
    shared: &Arc<Mutex<OpsModel>>,
    status: &Arc<Mutex<Option<String>>>,
    action_tx: UnboundedSender<GatewayAction>,
    run_tx: UnboundedSender<RunAction>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let result = Terminal::new(backend)
        .map_err(anyhow::Error::from)
        .and_then(|mut terminal| tui_loop(&mut terminal, shared, status, action_tx, run_tx));
    let cleanup = (|| -> Result<()> {
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, Show)?;
        Ok(())
    })();
    result.and(cleanup)
}

/// A pending `a`/`d` press on the Approvals tab, waiting on its inline
/// `y`/`n` confirmation (issue #52's acceptance criteria: "both actions
/// require an inline confirmation step before executing").
struct PendingConfirm {
    approve: bool,
    call_id: String,
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shared: &Arc<Mutex<OpsModel>>,
    status: &Arc<Mutex<Option<String>>>,
    action_tx: UnboundedSender<GatewayAction>,
    run_tx: UnboundedSender<RunAction>,
) -> Result<()> {
    let titles = ["Builds", "Sessions", "Traces", "Cost", "Approvals"];
    const BUILDS_TAB: usize = 0;
    const APPROVALS_TAB: usize = 4;
    let mut selected = 0;
    let mut build_cursor = 0usize;
    let mut approval_cursor = 0usize;
    let mut pending: Option<PendingConfirm> = None;
    loop {
        let (pages, builds, approval_lines, status_text) = {
            let model = shared.lock().unwrap();
            let pages = [
                model.builds_text(),
                model.sessions_text(),
                model.traces_text(),
                model.costs_text(),
            ];
            let builds = model.builds.clone();
            build_cursor = build_cursor.min(builds.len().saturating_sub(1));
            let approval_lines = model.approval_lines();
            approval_cursor = approval_cursor.min(approval_lines.len().saturating_sub(1));
            (
                pages,
                builds,
                approval_lines,
                status.lock().unwrap().clone(),
            )
        };
        terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(frame.area());
            let tabs = Tabs::new(titles.iter().map(|title| Line::from(*title)))
                .select(selected)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Cybersin ops · ←/→ switch · q quit "),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_widget(tabs, areas[0]);

            if selected == BUILDS_TAB {
                let mut state = ListState::default();
                if !builds.is_empty() {
                    state.select(Some(build_cursor));
                }
                let items: Vec<ListItem> = if builds.is_empty() {
                    vec![ListItem::new("no agents found in agents/**/*.agent.yaml")]
                } else {
                    builds
                        .iter()
                        .map(|build| {
                            ListItem::new(format!(
                                "{:<24} {:<12} {}",
                                build.name,
                                build.build_hash_short.as_deref().unwrap_or("unbuilt"),
                                build.path.display()
                            ))
                        })
                        .collect()
                };
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Builds "))
                    .highlight_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    );
                frame.render_stateful_widget(list, areas[1], &mut state);
            } else if selected == APPROVALS_TAB {
                let mut state = ListState::default();
                if !approval_lines.is_empty() {
                    state.select(Some(approval_cursor));
                }
                let items: Vec<ListItem> = if approval_lines.is_empty() {
                    vec![ListItem::new("no calls awaiting approval")]
                } else {
                    approval_lines
                        .iter()
                        .map(|line| ListItem::new(line.as_str()))
                        .collect()
                };
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" Approvals "))
                    .highlight_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    );
                frame.render_stateful_widget(list, areas[1], &mut state);
            } else {
                frame.render_widget(
                    Paragraph::new(pages[selected].as_str())
                        .block(Block::default().borders(Borders::ALL))
                        .wrap(Wrap { trim: false }),
                    areas[1],
                );
            }

            let footer = if let Some(pending) = &pending {
                format!(
                    "{} {}? [y/n]",
                    if pending.approve { "Approve" } else { "Deny" },
                    pending.call_id
                )
            } else if selected == APPROVALS_TAB {
                status_text
                    .clone()
                    .unwrap_or_else(|| "↑/↓ select · a approve · d deny".to_string())
            } else if selected == BUILDS_TAB {
                status_text
                    .clone()
                    .unwrap_or_else(|| "↑/↓ select · Enter run selected build".to_string())
            } else {
                status_text.clone().unwrap_or_default()
            };
            frame.render_widget(Paragraph::new(footer), areas[2]);
        })?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if let Some(confirm) = pending.take() {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Enter => {
                            let action = if confirm.approve {
                                GatewayAction::Approve(confirm.call_id)
                            } else {
                                GatewayAction::Deny(confirm.call_id)
                            };
                            let _ = action_tx.send(action);
                        }
                        _ => {} // 'n', Esc, or anything else cancels
                    }
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Left => selected = selected.saturating_sub(1),
                    KeyCode::Right => selected = (selected + 1).min(titles.len() - 1),
                    KeyCode::Char('1'..='5') => {
                        if let KeyCode::Char(number) = key.code {
                            selected = number as usize - '1' as usize;
                        }
                    }
                    KeyCode::Up if selected == BUILDS_TAB => {
                        build_cursor = build_cursor.saturating_sub(1);
                    }
                    KeyCode::Down if selected == BUILDS_TAB => {
                        if !builds.is_empty() {
                            build_cursor = (build_cursor + 1).min(builds.len() - 1);
                        }
                    }
                    KeyCode::Enter if selected == BUILDS_TAB => {
                        if let Some(build) = builds.get(build_cursor) {
                            let _ = run_tx.send(RunAction::Run(build.path.clone()));
                        }
                    }
                    KeyCode::Up if selected == APPROVALS_TAB => {
                        approval_cursor = approval_cursor.saturating_sub(1);
                    }
                    KeyCode::Down if selected == APPROVALS_TAB => {
                        if !approval_lines.is_empty() {
                            approval_cursor = (approval_cursor + 1).min(approval_lines.len() - 1);
                        }
                    }
                    KeyCode::Char('a') if selected == APPROVALS_TAB => {
                        if let Some(call_id) = approval_call_id(shared, approval_cursor) {
                            pending = Some(PendingConfirm {
                                approve: true,
                                call_id,
                            });
                        }
                    }
                    KeyCode::Char('d') if selected == APPROVALS_TAB => {
                        if let Some(call_id) = approval_call_id(shared, approval_cursor) {
                            pending = Some(PendingConfirm {
                                approve: false,
                                call_id,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The call id at `index` in the Approvals tab's current rows, re-read
/// from `shared` at press-time rather than trusting a possibly-stale
/// snapshot from the last draw — a background refresh can reorder or
/// shrink the list between frames.
fn approval_call_id(shared: &Arc<Mutex<OpsModel>>, index: usize) -> Option<String> {
    shared
        .lock()
        .unwrap()
        .approvals
        .get(index)
        .map(|row| row.call_id.clone())
}
