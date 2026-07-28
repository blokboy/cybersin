//! Bare `cybersin`: a reusable Ratatui application shell. `Home` is just
//! the control-room backdrop plus a one-line hint — `Enter` drops
//! straight into the `Workspace` screen on its `Convert` tab, the
//! lowest-friction path into the shell. `Workspace` multiplexes
//! `Convert`/`Build`/`Ops` behind a `Tabs` bar (arrow keys switch tabs)
//! rather than adding a new top-level `Screen` per workflow — a later
//! workflow joins as another tab instead of another screen plus another
//! `go_back` case.
//!
//! `Build` and `Ops` are read-only info panels — `Build` also runs a
//! real, Dev-profile build on demand. The Ops tab shows a cached
//! snapshot of the same Builds/Sessions/Traces/Cost/Approvals sections
//! that `cybersin ops --plain` prints, plus any `dist/*.log` files from
//! local TUI builds.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::capabilities::{
    apply_safety_gate, build_summary, check_output_stream, execute_build, execute_check,
    execute_scaffold_build_with_progress, execute_trace_ls, registry, AdapterSupport,
    CapabilityEvent, CapabilitySpec, SafetyGateResult, ScaffoldBuildInput, TraceLsInput,
    BUILD_CAPABILITY_ID, CHECK_CAPABILITY_ID, SCAFFOLD_BUILD_WORKFLOW_ID, TRACE_LS_CAPABILITY_ID,
};
#[cfg(test)]
use crate::commands::build;
use crate::commands::build::{BuildProfile, BuildProgress};
use crate::commands::convert::{
    self, ConvertReport, OpenRouterPromptConversionModel, PromptConversionModel,
};
use crate::commands::ops;
use crate::commands::run::{self, RunArgs};
use crate::project::ProjectDefaults;
use crate::project::{self};
use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use cybersin_runtime::DaemonHandle;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Workspace,
    CapabilityBrowser,
}

/// The tab selected inside `Screen::Workspace`. `Convert` is the only
/// one with editable fields; `Build` always sits at `Focus::Navigation`.
/// `Ops` sits at `Focus::Navigation` too except when Tab has moved focus
/// into the adjacent Builds list (`Focus::OpsBuildsList`), which only
/// happens while its "Builds" row is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceTab {
    Convert,
    Build,
    Ops,
}

impl WorkspaceTab {
    const ALL: [WorkspaceTab; 3] = [
        WorkspaceTab::Convert,
        WorkspaceTab::Build,
        WorkspaceTab::Ops,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap()
    }

    fn title(self) -> &'static str {
        match self {
            WorkspaceTab::Convert => "Convert",
            WorkspaceTab::Build => "Build",
            WorkspaceTab::Ops => "Ops",
        }
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Navigation,
    Prompt,
    Model,
    Out,
    ConvertAction,
    OpsBuildsList,
    CapabilityForm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConversionStatus {
    Idle,
    Running,
    Success(ConvertSummary),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConvertSummary {
    path: PathBuf,
    inputs: Vec<String>,
    tools: Vec<String>,
    unmapped_sections: Vec<String>,
}

impl From<ConvertReport> for ConvertSummary {
    fn from(report: ConvertReport) -> Self {
        Self {
            path: report.path,
            inputs: report.inputs,
            tools: report.tools,
            unmapped_sections: report.unmapped_sections,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BuildStatus {
    Idle,
    Running(Vec<String>),
    Success(String),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OpsStatus {
    Idle,
    Running,
    Success(Vec<OpsEntry>),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpsEntry {
    title: String,
    body: String,
}

/// Outcome of running a build from the Ops tab's Builds list — separate
/// from `BuildStatus`, which tracks the `Build` tab's *compile* action;
/// this tracks *executing* an already-compiled agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpsRunStatus {
    Idle,
    Running,
    Success(String),
    Failure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CapabilityStatus {
    Idle,
    Running(String),
    Success(String),
    Failure(String),
}

#[derive(Debug)]
struct App {
    project_start: PathBuf,
    screen: Screen,
    workspace_tab: WorkspaceTab,
    focus: Focus,
    raw_prompt: String,
    model: String,
    out: String,
    convert_status: ConversionStatus,
    build_status: BuildStatus,
    selected_build_source: usize,
    ops_status: OpsStatus,
    selected_ops_entry: usize,
    ops_builds: Vec<ops::OpsBuild>,
    selected_ops_build: usize,
    ops_run_status: OpsRunStatus,
    selected_capability: usize,
    capability_form: CapabilityFormState,
    capability_status: CapabilityStatus,
    show_help: bool,
    should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        let project_start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(project_start)
    }
}

impl App {
    fn new(project_start: PathBuf) -> Self {
        let project_start = resolve_tui_project_start(&project_start);
        let capability_form = CapabilityFormState::for_current_selection(&project_start, 0);
        Self {
            project_start,
            screen: Screen::Home,
            workspace_tab: WorkspaceTab::Convert,
            focus: Focus::Navigation,
            raw_prompt: String::new(),
            model: convert::DEFAULT_MODEL.to_string(),
            out: String::new(),
            convert_status: ConversionStatus::Idle,
            build_status: BuildStatus::Idle,
            selected_build_source: 0,
            ops_status: OpsStatus::Idle,
            selected_ops_entry: 0,
            ops_builds: Vec::new(),
            selected_ops_build: 0,
            ops_run_status: OpsRunStatus::Idle,
            selected_capability: 0,
            capability_form,
            capability_status: CapabilityStatus::Idle,
            show_help: false,
            should_quit: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityFormState {
    capability_id: String,
    fields: Vec<CapabilityFormField>,
    selected_field: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapabilityFormField {
    name: String,
    description: String,
    required: bool,
    kind: CapabilityFormFieldKind,
    value: CapabilityFormValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CapabilityFormFieldKind {
    String { nullable: bool },
    Boolean,
    Enum { values: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CapabilityFormValue {
    Text(String),
    Boolean(bool),
    Enum(usize),
}

impl CapabilityFormState {
    fn for_current_selection(project_start: &Path, selected_capability: usize) -> Self {
        let registry = registry();
        let selected = selected_capability.min(registry.specs().len().saturating_sub(1));
        registry
            .specs()
            .get(selected)
            .map(|spec| Self::from_schema(project_start, spec))
            .unwrap_or_else(|| Self {
                capability_id: String::new(),
                fields: Vec::new(),
                selected_field: 0,
            })
    }

    fn sync_selection(&mut self, project_start: &Path, selected_capability: usize) {
        let next = Self::for_current_selection(project_start, selected_capability);
        if self.capability_id != next.capability_id {
            *self = next;
        }
    }

    fn from_schema(project_start: &Path, spec: &CapabilitySpec) -> Self {
        let required = schema_required_fields(&spec.input_schema);
        let mut fields = Vec::new();
        if let Some(properties) = spec
            .input_schema
            .get("properties")
            .and_then(Value::as_object)
        {
            for (name, property) in properties {
                if let Some(kind) = CapabilityFormFieldKind::from_schema(property) {
                    let required = required.iter().any(|field| field == name);
                    let value = default_capability_form_value(project_start, spec, name, &kind);
                    fields.push(CapabilityFormField {
                        name: name.clone(),
                        description: property
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        required,
                        kind,
                        value,
                    });
                }
            }
        }
        Self {
            capability_id: spec.id.as_str().to_string(),
            fields,
            selected_field: 0,
        }
    }

    fn selected_field_mut(&mut self) -> Option<&mut CapabilityFormField> {
        let selected = self.selected_field.min(self.fields.len().saturating_sub(1));
        self.fields.get_mut(selected)
    }

    fn move_field_up(&mut self) {
        self.selected_field = self.selected_field.saturating_sub(1);
    }

    fn move_field_down(&mut self) {
        self.selected_field = (self.selected_field + 1).min(self.fields.len().saturating_sub(1));
    }

    fn insert_char(&mut self, ch: char) {
        let Some(field) = self.selected_field_mut() else {
            return;
        };
        match &mut field.value {
            CapabilityFormValue::Text(value) => value.push(ch),
            CapabilityFormValue::Boolean(value) => {
                if ch == ' ' {
                    *value = !*value;
                }
            }
            CapabilityFormValue::Enum(index) => {
                if ch == ' ' {
                    *index = (*index + 1) % enum_value_count(&field.kind).max(1);
                }
            }
        }
    }

    fn backspace(&mut self) {
        let Some(field) = self.selected_field_mut() else {
            return;
        };
        if let CapabilityFormValue::Text(value) = &mut field.value {
            value.pop();
        }
    }

    fn cycle_selected_value(&mut self, direction: CapabilityCycleDirection) {
        let Some(field) = self.selected_field_mut() else {
            return;
        };
        match (&field.kind, &mut field.value) {
            (CapabilityFormFieldKind::Boolean, CapabilityFormValue::Boolean(value)) => {
                *value = !*value;
            }
            (CapabilityFormFieldKind::Enum { values }, CapabilityFormValue::Enum(index))
                if !values.is_empty() =>
            {
                *index = match direction {
                    CapabilityCycleDirection::Next => (*index + 1) % values.len(),
                    CapabilityCycleDirection::Previous => {
                        (*index + values.len() - 1) % values.len()
                    }
                };
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityCycleDirection {
    Previous,
    Next,
}

impl CapabilityFormFieldKind {
    fn from_schema(schema: &Value) -> Option<Self> {
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let values = values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>();
            if !values.is_empty() {
                return Some(Self::Enum { values });
            }
        }
        if schema_type_includes(schema, "boolean") {
            return Some(Self::Boolean);
        }
        if schema_type_includes(schema, "string") {
            return Some(Self::String {
                nullable: schema_type_includes(schema, "null"),
            });
        }
        None
    }
}

fn enum_value_count(kind: &CapabilityFormFieldKind) -> usize {
    match kind {
        CapabilityFormFieldKind::Enum { values } => values.len(),
        _ => 0,
    }
}

fn resolve_tui_project_start(project_start: &Path) -> PathBuf {
    project::discover_project_root(project_start)
        .or_else(|| discover_single_descendant_project(project_start))
        .unwrap_or_else(|| project_start.to_path_buf())
}

fn discover_single_descendant_project(start: &Path) -> Option<PathBuf> {
    let mut found = Vec::new();
    collect_descendant_projects(start, 0, &mut found);
    found.sort();
    found.dedup();
    if found.len() == 1 {
        found.pop()
    } else {
        None
    }
}

fn collect_descendant_projects(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    const MAX_DEPTH: usize = 4;
    const MAX_FOUND: usize = 2;
    if depth > MAX_DEPTH || found.len() >= MAX_FOUND {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if found.len() >= MAX_FOUND {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() || should_skip_project_scan_dir(&path) {
            continue;
        }
        if path.join("cybersin.yaml").is_file() {
            found.push(path);
        } else {
            collect_descendant_projects(&path, depth + 1, found);
        }
    }
}

fn should_skip_project_scan_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with('.') || matches!(name, "target" | "node_modules" | "dist")
}

fn schema_required_fields(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn schema_type_includes(schema: &Value, expected: &str) -> bool {
    match schema.get("type") {
        Some(Value::String(value)) => value == expected,
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some(expected)),
        _ => false,
    }
}

fn default_capability_form_value(
    project_start: &Path,
    spec: &CapabilitySpec,
    name: &str,
    kind: &CapabilityFormFieldKind,
) -> CapabilityFormValue {
    match kind {
        CapabilityFormFieldKind::Boolean => {
            CapabilityFormValue::Boolean(default_capability_boolean(spec, name))
        }
        CapabilityFormFieldKind::Enum { values } => {
            let default = default_capability_enum(spec, name);
            let index = default
                .and_then(|default| values.iter().position(|value| value == default))
                .unwrap_or(0);
            CapabilityFormValue::Enum(index)
        }
        CapabilityFormFieldKind::String { .. } => CapabilityFormValue::Text(
            default_capability_string(project_start, spec, name).unwrap_or_default(),
        ),
    }
}

fn default_capability_string(
    project_start: &Path,
    spec: &CapabilitySpec,
    name: &str,
) -> Option<String> {
    match (spec.id.as_str(), name) {
        (BUILD_CAPABILITY_ID, "project_path") | (CHECK_CAPABILITY_ID, "path") => Some(
            resolve_tui_project_start(project_start)
                .display()
                .to_string(),
        ),
        _ => None,
    }
}

fn default_capability_boolean(_spec: &CapabilitySpec, _name: &str) -> bool {
    false
}

fn default_capability_enum(spec: &CapabilitySpec, name: &str) -> Option<&'static str> {
    match (spec.id.as_str(), name) {
        (BUILD_CAPABILITY_ID, "profile") => Some("dev"),
        _ => None,
    }
}

#[derive(Debug)]
enum AppAction {
    None,
    Convert,
    Build,
    RefreshOps,
    RunOpsBuild(PathBuf),
    ExecuteCapability,
}

impl App {
    fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.kind != KeyEventKind::Press {
            return AppAction::None;
        }
        if self.show_help {
            self.show_help = false;
            return AppAction::None;
        }
        match key.code {
            KeyCode::Char('?') => {
                self.show_help = true;
                AppAction::None
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.request_action()
            }
            KeyCode::F(5) => self.request_action(),
            KeyCode::Char('b') if self.screen == Screen::Home => {
                self.enter_capability_browser();
                AppAction::None
            }
            KeyCode::Esc => {
                self.go_back();
                AppAction::None
            }
            KeyCode::Left if self.screen == Screen::Workspace => {
                let tab = self.workspace_tab.previous();
                self.switch_tab(tab);
                self.post_switch_action()
            }
            KeyCode::Right if self.screen == Screen::Workspace => {
                let tab = self.workspace_tab.next();
                self.switch_tab(tab);
                self.post_switch_action()
            }
            KeyCode::Tab => {
                self.focus_next();
                AppAction::None
            }
            KeyCode::BackTab => {
                self.focus_previous();
                AppAction::None
            }
            KeyCode::Up if self.screen == Screen::Workspace => {
                self.move_selection_up();
                AppAction::None
            }
            KeyCode::Down if self.screen == Screen::Workspace => {
                self.move_selection_down();
                AppAction::None
            }
            KeyCode::Up if self.screen == Screen::CapabilityBrowser => {
                if self.focus == Focus::CapabilityForm {
                    self.capability_form.move_field_up();
                } else {
                    self.selected_capability = self.selected_capability.saturating_sub(1);
                    self.capability_form
                        .sync_selection(&self.project_start, self.selected_capability);
                    self.capability_status = CapabilityStatus::Idle;
                }
                AppAction::None
            }
            KeyCode::Down if self.screen == Screen::CapabilityBrowser => {
                if self.focus == Focus::CapabilityForm {
                    self.capability_form.move_field_down();
                } else {
                    let max_index = registry().specs().len().saturating_sub(1);
                    self.selected_capability = (self.selected_capability + 1).min(max_index);
                    self.capability_form
                        .sync_selection(&self.project_start, self.selected_capability);
                    self.capability_status = CapabilityStatus::Idle;
                }
                AppAction::None
            }
            KeyCode::Left
                if self.screen == Screen::CapabilityBrowser
                    && self.focus == Focus::CapabilityForm =>
            {
                self.capability_form
                    .cycle_selected_value(CapabilityCycleDirection::Previous);
                self.capability_status = CapabilityStatus::Idle;
                AppAction::None
            }
            KeyCode::Right
                if self.screen == Screen::CapabilityBrowser
                    && self.focus == Focus::CapabilityForm =>
            {
                self.capability_form
                    .cycle_selected_value(CapabilityCycleDirection::Next);
                self.capability_status = CapabilityStatus::Idle;
                AppAction::None
            }
            KeyCode::Char('q')
                if self.focus != Focus::Prompt && self.focus != Focus::CapabilityForm =>
            {
                self.should_quit = true;
                AppAction::None
            }
            KeyCode::Enter if self.screen == Screen::Home => {
                self.enter_convert();
                AppAction::None
            }
            KeyCode::Enter if self.screen == Screen::CapabilityBrowser => self.request_action(),
            KeyCode::Enter if self.workspace_tab == WorkspaceTab::Build => self.request_action(),
            KeyCode::Enter
                if self.workspace_tab == WorkspaceTab::Ops
                    && self.focus == Focus::OpsBuildsList =>
            {
                self.request_run_selected_build()
            }
            KeyCode::Enter if self.workspace_tab == WorkspaceTab::Ops => self.request_action(),
            KeyCode::Enter if self.focus == Focus::ConvertAction => self.request_action(),
            KeyCode::Enter if self.focus == Focus::Prompt => {
                self.raw_prompt.push('\n');
                self.convert_status = ConversionStatus::Idle;
                AppAction::None
            }
            KeyCode::Char('c')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.focus != Focus::Prompt =>
            {
                self.should_quit = true;
                AppAction::None
            }
            KeyCode::Backspace => {
                self.backspace();
                AppAction::None
            }
            KeyCode::Char(ch) => {
                self.insert_char(ch);
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn enter_convert(&mut self) {
        self.screen = Screen::Workspace;
        self.switch_tab(WorkspaceTab::Convert);
    }

    fn enter_capability_browser(&mut self) {
        self.screen = Screen::CapabilityBrowser;
        self.focus = Focus::Navigation;
        self.capability_form
            .sync_selection(&self.project_start, self.selected_capability);
    }

    fn switch_tab(&mut self, tab: WorkspaceTab) {
        self.workspace_tab = tab;
        self.focus = match tab {
            WorkspaceTab::Convert => Focus::Prompt,
            WorkspaceTab::Build | WorkspaceTab::Ops => Focus::Navigation,
        };
    }

    fn post_switch_action(&mut self) -> AppAction {
        if self.workspace_tab == WorkspaceTab::Ops && self.ops_status == OpsStatus::Idle {
            AppAction::RefreshOps
        } else {
            AppAction::None
        }
    }

    fn request_action(&mut self) -> AppAction {
        if self.screen == Screen::CapabilityBrowser {
            return AppAction::ExecuteCapability;
        }
        if self.screen != Screen::Workspace {
            return AppAction::None;
        }
        match self.workspace_tab {
            WorkspaceTab::Convert => {
                if self.raw_prompt.trim().is_empty() {
                    self.convert_status =
                        ConversionStatus::Failure("Enter a prompt before converting.".to_string());
                    AppAction::None
                } else {
                    AppAction::Convert
                }
            }
            WorkspaceTab::Build => AppAction::Build,
            WorkspaceTab::Ops => AppAction::RefreshOps,
        }
    }

    /// `Enter` while focus has tabbed onto the Ops tab's Builds list —
    /// runs whichever build is currently highlighted there.
    fn request_run_selected_build(&mut self) -> AppAction {
        match self.ops_builds.get(self.selected_ops_build) {
            Some(build) => AppAction::RunOpsBuild(build.path.clone()),
            None => AppAction::None,
        }
    }

    /// Whether the Ops tab's left-hand entry list currently has "Builds"
    /// highlighted — the only row Tab is allowed to move focus off of,
    /// into the adjacent Builds list.
    fn selected_ops_entry_is_builds(&self) -> bool {
        matches!(&self.ops_status, OpsStatus::Success(entries) if entries
            .get(self.selected_ops_entry)
            .is_some_and(|entry| entry.title == "Builds"))
    }

    /// Toggles focus between the Ops tab's entry list and its Builds
    /// list — the only two focus stops on that tab, so `Tab` and
    /// `Shift-Tab` behave identically here.
    fn ops_toggle_focus(&mut self) {
        self.focus = if self.focus == Focus::OpsBuildsList {
            Focus::Navigation
        } else if self.selected_ops_entry_is_builds() {
            Focus::OpsBuildsList
        } else {
            Focus::Navigation
        };
    }

    fn go_back(&mut self) {
        match self.screen {
            Screen::Home => self.should_quit = true,
            Screen::Workspace => {
                self.screen = Screen::Home;
                self.focus = Focus::Navigation;
            }
            Screen::CapabilityBrowser => {
                self.screen = Screen::Home;
                self.focus = Focus::Navigation;
            }
        }
    }

    fn focus_next(&mut self) {
        if self.screen == Screen::CapabilityBrowser {
            self.focus = if self.focus == Focus::CapabilityForm {
                Focus::Navigation
            } else {
                Focus::CapabilityForm
            };
            return;
        }
        if self.screen != Screen::Workspace {
            return;
        }
        match self.workspace_tab {
            WorkspaceTab::Convert => {
                self.focus = match self.focus {
                    Focus::Prompt => Focus::Model,
                    Focus::Model => Focus::Out,
                    Focus::Out => Focus::ConvertAction,
                    _ => Focus::Prompt,
                };
            }
            WorkspaceTab::Ops => self.ops_toggle_focus(),
            WorkspaceTab::Build => {}
        }
    }

    fn focus_previous(&mut self) {
        if self.screen == Screen::CapabilityBrowser {
            self.focus_next();
            return;
        }
        if self.screen != Screen::Workspace {
            return;
        }
        match self.workspace_tab {
            WorkspaceTab::Convert => {
                self.focus = match self.focus {
                    Focus::Prompt => Focus::ConvertAction,
                    Focus::Model => Focus::Prompt,
                    Focus::Out => Focus::Model,
                    _ => Focus::Out,
                };
            }
            WorkspaceTab::Ops => self.ops_toggle_focus(),
            WorkspaceTab::Build => {}
        }
    }

    fn insert_char(&mut self, ch: char) {
        if self.screen == Screen::CapabilityBrowser && self.focus == Focus::CapabilityForm {
            self.capability_form.insert_char(ch);
            self.capability_status = CapabilityStatus::Idle;
            return;
        }
        match self.focus {
            Focus::Prompt => self.raw_prompt.push(ch),
            Focus::Model => self.model.push(ch),
            Focus::Out => self.out.push(ch),
            _ => return,
        }
        self.convert_status = ConversionStatus::Idle;
    }

    fn backspace(&mut self) {
        if self.screen == Screen::CapabilityBrowser && self.focus == Focus::CapabilityForm {
            self.capability_form.backspace();
            self.capability_status = CapabilityStatus::Idle;
            return;
        }
        match self.focus {
            Focus::Prompt => {
                self.raw_prompt.pop();
            }
            Focus::Model => {
                self.model.pop();
            }
            Focus::Out => {
                self.out.pop();
            }
            _ => return,
        }
        self.convert_status = ConversionStatus::Idle;
    }

    fn move_selection_up(&mut self) {
        match self.workspace_tab {
            WorkspaceTab::Build => {
                self.selected_build_source = self.selected_build_source.saturating_sub(1);
            }
            WorkspaceTab::Ops if self.focus == Focus::OpsBuildsList => {
                self.selected_ops_build = self.selected_ops_build.saturating_sub(1);
            }
            WorkspaceTab::Ops => {
                self.selected_ops_entry = self.selected_ops_entry.saturating_sub(1);
            }
            WorkspaceTab::Convert => {}
        }
    }

    fn move_selection_down(&mut self) {
        match self.workspace_tab {
            WorkspaceTab::Build => {
                let max_index = build_sources(&self.project_start)
                    .map(|sources| sources.len().saturating_sub(1))
                    .unwrap_or(0);
                self.selected_build_source = (self.selected_build_source + 1).min(max_index);
            }
            WorkspaceTab::Ops if self.focus == Focus::OpsBuildsList => {
                let max_index = self.ops_builds.len().saturating_sub(1);
                self.selected_ops_build = (self.selected_ops_build + 1).min(max_index);
            }
            WorkspaceTab::Ops => {
                let max_index = match &self.ops_status {
                    OpsStatus::Success(entries) => entries.len().saturating_sub(1),
                    _ => 0,
                };
                self.selected_ops_entry = (self.selected_ops_entry + 1).min(max_index);
            }
            WorkspaceTab::Convert => {}
        }
    }

    fn conversion_out(&self) -> Option<PathBuf> {
        let trimmed = self.out.trim();
        (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
    }
}

pub async fn execute(project_start: PathBuf) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "bare `cybersin` requires an interactive terminal; use `cybersin -help` or an explicit subcommand for non-interactive use"
        );
    }
    let mut app = App::new(project_start);
    run_terminal(&mut app).await
}

async fn run_terminal(app: &mut App) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    loop {
        terminal.draw(|frame| render(frame, app))?;
        if app.should_quit {
            break;
        }
        if let Event::Key(key) = event::read().context("reading terminal input")? {
            match app.handle_key(key) {
                AppAction::Convert => {
                    app.convert_status = ConversionStatus::Running;
                    terminal.draw(|frame| render(frame, app))?;
                    let result = run_conversion(app).await;
                    app.convert_status = match result {
                        Ok(report) => ConversionStatus::Success(report.into()),
                        Err(error) => ConversionStatus::Failure(error),
                    };
                }
                AppAction::Build => {
                    run_build_interactive(&mut terminal, app)?;
                }
                AppAction::RefreshOps => {
                    app.ops_status = OpsStatus::Running;
                    terminal.draw(|frame| render(frame, app))?;
                    app.ops_status = match load_ops_entries(&app.project_start).await {
                        Ok(entries) => OpsStatus::Success(entries),
                        Err(error) => OpsStatus::Failure(error),
                    };
                    clamp_selected_ops_entry(app);
                    app.ops_builds = load_ops_builds(&app.project_start).unwrap_or_default();
                    clamp_selected_ops_build(app);
                }
                AppAction::RunOpsBuild(agent_yaml) => {
                    app.ops_run_status = OpsRunStatus::Running;
                    terminal.draw(|frame| render(frame, app))?;
                    let result = run_ops_build(&app.project_start, agent_yaml).await;
                    app.ops_run_status = match result {
                        Ok(message) => OpsRunStatus::Success(message),
                        Err(error) => OpsRunStatus::Failure(error),
                    };
                }
                AppAction::ExecuteCapability => {
                    let Some(spec) = selected_capability_spec(app) else {
                        app.capability_status =
                            CapabilityStatus::Failure("No capability selected.".to_string());
                        continue;
                    };
                    app.capability_status =
                        CapabilityStatus::Running(format!("Running {}", spec.id.as_str()));
                    terminal.draw(|frame| render(frame, app))?;
                    app.capability_status = match run_generic_capability(
                        &app.project_start,
                        &spec,
                        &app.capability_form,
                    )
                    .await
                    {
                        Ok(output) => CapabilityStatus::Success(output),
                        Err(error) => CapabilityStatus::Failure(error),
                    };
                }
                AppAction::None => {}
            }
        }
    }
    Ok(())
}

fn selected_capability_spec(app: &App) -> Option<CapabilitySpec> {
    let registry = registry();
    let selected = app
        .selected_capability
        .min(registry.specs().len().saturating_sub(1));
    registry.specs().get(selected).cloned()
}

async fn run_generic_capability(
    project_start: &Path,
    spec: &CapabilitySpec,
    form: &CapabilityFormState,
) -> Result<String, String> {
    match &spec.adapters.tui {
        AdapterSupport::Generic => {}
        AdapterSupport::Unavailable { reason } => return Err(reason.clone()),
        AdapterSupport::Available | AdapterSupport::Custom => {
            return Err("this capability is not available through the generic browser".to_string())
        }
    }

    let input = normalize_capability_input(project_start, spec, form)?;
    let mut events = tui_safety_events(spec)?;
    let execution_events = match spec.id.as_str() {
        CHECK_CAPABILITY_ID => {
            let path = input_path(&input, "path")?;
            execute_check(crate::capabilities::CheckInput::new(path)).events
        }
        BUILD_CAPABILITY_ID => {
            let project_path = input_path(&input, "project_path")?;
            let profile = match input
                .get("profile")
                .and_then(Value::as_str)
                .ok_or_else(|| "profile is required".to_string())?
            {
                "dev" => BuildProfile::Dev,
                "release" => BuildProfile::Release,
                other => return Err(format!("profile must be dev or release, got {other}")),
            };
            let frozen = input
                .get("frozen")
                .and_then(Value::as_bool)
                .ok_or_else(|| "frozen is required".to_string())?;
            let mut build_input =
                crate::capabilities::BuildInput::new(project_path, profile, frozen);
            if let Some(source) = input.get("selected_prompt_source").and_then(Value::as_str) {
                build_input = build_input.with_selected_prompt_source(source);
            }
            execute_build(build_input).events
        }
        TRACE_LS_CAPABILITY_ID => {
            let project_root = resolve_project_root(project_start)?;
            let defaults =
                ProjectDefaults::detect(&project_root).map_err(|error| error.to_string())?;
            let daemon = DaemonHandle::auto_start(&defaults.db_default())
                .await
                .map_err(|error| error.to_string())?;
            execute_trace_ls(
                &daemon.spans(),
                TraceLsInput {
                    session: input
                        .get("session")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    agent: input
                        .get("agent")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    model: input
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    limit: optional_u32(&input, "limit")?.or(Some(25)),
                },
            )
            .await
            .events
        }
        _ => {
            return Err(
                "this capability is listed but not wired into the generic browser yet".to_string(),
            )
        }
    };
    events.extend(execution_events);

    Ok(render_capability_events(&events))
}

fn tui_safety_events(spec: &CapabilitySpec) -> Result<Vec<CapabilityEvent>, String> {
    match apply_safety_gate(spec, false, |_decision, _message| {
        unreachable!("the bare TUI has no confirmation prompt yet")
    }) {
        SafetyGateResult::Accepted { events } => Ok(events),
        SafetyGateResult::Blocked { execution } => Err(render_capability_events(&execution.events)),
    }
}

fn normalize_capability_input(
    project_start: &Path,
    spec: &CapabilitySpec,
    form: &CapabilityFormState,
) -> Result<Value, String> {
    if form.capability_id != spec.id.as_str() {
        return normalize_capability_input(
            project_start,
            spec,
            &CapabilityFormState::from_schema(project_start, spec),
        );
    }
    let mut object = serde_json::Map::new();
    for field in &form.fields {
        match normalized_field_value(field) {
            Some(value) => {
                object.insert(field.name.clone(), value);
            }
            None if field.required => {
                return Err(format!("{} is required", field.name));
            }
            None => {}
        }
    }
    Ok(Value::Object(object))
}

fn normalized_field_value(field: &CapabilityFormField) -> Option<Value> {
    match (&field.kind, &field.value) {
        (CapabilityFormFieldKind::String { nullable }, CapabilityFormValue::Text(value)) => {
            let value = value.trim();
            if value.is_empty() {
                if *nullable {
                    Some(Value::Null)
                } else {
                    None
                }
            } else {
                Some(Value::String(value.to_string()))
            }
        }
        (CapabilityFormFieldKind::Boolean, CapabilityFormValue::Boolean(value)) => {
            Some(Value::Bool(*value))
        }
        (CapabilityFormFieldKind::Enum { values }, CapabilityFormValue::Enum(index)) => {
            values.get(*index).map(|value| Value::String(value.clone()))
        }
        _ => None,
    }
}

fn input_path(input: &Value, key: &str) -> Result<PathBuf, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{key} is required"))
}

fn optional_u32(input: &Value, key: &str) -> Result<Option<u32>, String> {
    match input.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => value
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| format!("{key} must be a non-negative integer")),
        _ => Ok(None),
    }
}

fn render_capability_events(events: &[CapabilityEvent]) -> String {
    let mut lines = Vec::new();
    for event in events {
        match event {
            CapabilityEvent::Started { capability_id } => {
                lines.push(format!("started {}", capability_id.as_str()));
            }
            CapabilityEvent::Progress {
                message,
                current,
                total,
            } => {
                let prefix = match (current, total) {
                    (Some(current), Some(total)) => format!("[{current}/{total}] "),
                    _ => String::new(),
                };
                lines.push(format!("{prefix}{message}"));
            }
            CapabilityEvent::Prompt { message, .. } => {
                lines.push(format!("prompt: {message}"));
            }
            CapabilityEvent::Output {
                mode: crate::capabilities::OutputMode::Text,
                value,
            } => {
                if let Some((_stream, text)) = check_output_stream(value) {
                    lines.push(text.trim_end().to_string());
                }
            }
            CapabilityEvent::Output { mode, .. } => {
                lines.push(format!("output: {mode:?}"));
            }
            CapabilityEvent::Completed { value } => {
                if let Some(value) = value {
                    lines.push(format!("completed: {value}"));
                } else {
                    lines.push("completed".to_string());
                }
            }
            CapabilityEvent::Failed { message } => {
                lines.push(format!("failed: {message}"));
            }
        }
    }
    lines.join("\n")
}

async fn load_ops_entries(project_start: &Path) -> Result<Vec<OpsEntry>, String> {
    let project_root = resolve_project_root(project_start)?;
    let sections = ops::plain_sections_for_path(&project_root)
        .await
        .map_err(|error| error.to_string())?;
    let mut entries = ops_log_entries(&project_root);
    entries.extend([
        OpsEntry {
            title: "Builds".to_string(),
            body: sections.builds,
        },
        OpsEntry {
            title: "Sessions".to_string(),
            body: sections.sessions,
        },
        OpsEntry {
            title: "Traces".to_string(),
            body: sections.traces,
        },
        OpsEntry {
            title: "Cost".to_string(),
            body: sections.cost,
        },
        OpsEntry {
            title: "Approvals".to_string(),
            body: sections.approvals,
        },
    ]);
    Ok(entries)
}

fn ops_log_entries(project_root: &Path) -> Vec<OpsEntry> {
    let dist = project_root.join("dist");
    let Ok(entries) = fs::read_dir(&dist) else {
        return Vec::new();
    };
    let mut logs = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("log")
        })
        .collect::<Vec<_>>();
    logs.sort();
    logs.into_iter()
        .map(|path| {
            let title = path
                .strip_prefix(project_root)
                .unwrap_or(&path)
                .display()
                .to_string();
            let body = fs::read_to_string(&path)
                .map(|text| tail_lines(&text, 24))
                .unwrap_or_else(|error| format!("error: reading {}: {error}", path.display()));
            OpsEntry { title, body }
        })
        .collect()
}

fn clamp_selected_ops_entry(app: &mut App) {
    let max_index = match &app.ops_status {
        OpsStatus::Success(entries) => entries.len().saturating_sub(1),
        _ => 0,
    };
    app.selected_ops_entry = app.selected_ops_entry.min(max_index);
}

fn clamp_selected_ops_build(app: &mut App) {
    let max_index = app.ops_builds.len().saturating_sub(1);
    app.selected_ops_build = app.selected_ops_build.min(max_index);
}

/// The Ops tab's Builds list — same discovery `cybersin ops`'s own
/// Builds tab uses (`commands::ops::discover_ops_builds`), so both
/// surfaces agree on what counts as a build.
fn load_ops_builds(project_start: &Path) -> Result<Vec<ops::OpsBuild>, String> {
    let project_root = resolve_project_root(project_start)?;
    ops::discover_ops_builds(&project_root).map_err(|error| error.to_string())
}

/// Runs an already-compiled agent (an Ops Builds list row) as a real
/// session — the same `run::run_session` path `cybersin run` and
/// `cybersin ops`'s own Builds tab use, wired to this project's default
/// db/dist/sandbox paths since the bare TUI has no flags to override them.
async fn run_ops_build(project_start: &Path, agent_yaml: PathBuf) -> Result<String, String> {
    let project_root = resolve_project_root(project_start)?;
    let defaults = ProjectDefaults::detect(&project_root).map_err(|error| error.to_string())?;
    let dist = defaults.dist_default().map_err(|error| error.to_string())?;
    let summary = run::run_session(
        defaults.db_default(),
        dist,
        defaults.sandbox_root_default(),
        defaults.sandbox_backend_default(),
        RunArgs {
            agent_yaml: Some(agent_yaml),
            stub: false,
            session_id: None,
            resume: None,
            force: false,
            agent: None,
            input: None,
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(format!(
        "{} {}: {} span(s)",
        summary.session_id,
        if summary.completed {
            "completed"
        } else {
            "aborted"
        },
        summary.spans_recorded
    ))
}

/// Resolves the project the bare shell is standing inside of — shared
/// by `Convert`'s and `Build`'s primary actions and by `Build`/`Ops`'s
/// display panels, so all four agree on the exact same discovery rule
/// (issue #50's walk-up-for-`cybersin.yaml`) instead of each hand-rolling
/// their own `current_dir` + ancestors search.
fn resolve_project_root(project_start: &Path) -> Result<PathBuf, String> {
    project::discover_project_root(project_start).ok_or_else(|| {
        format!(
            "error: no cybersin.yaml found in {} or any parent directory",
            project_start.display()
        )
    })
}

async fn run_conversion(app: &App) -> Result<ConvertReport, String> {
    let project_root = resolve_project_root(&app.project_start)?;
    ProjectDefaults::detect(&project_root)
        .map_err(|error| error.to_string())?
        .load_dotenv()
        .map_err(|error| error.to_string())?;
    let local_config = cybersin_runtime::LocalConfigFile::load_optional(&project_root)
        .map_err(|error| error.to_string())?;
    let converter = OpenRouterPromptConversionModel::from_local_config(
        app.model.trim().to_string(),
        local_config.as_ref(),
    )?;
    run_conversion_with_model(&converter, &project_root, app).await
}

async fn run_conversion_with_model(
    converter: &dyn PromptConversionModel,
    project_root: &Path,
    app: &App,
) -> Result<ConvertReport, String> {
    convert::run_raw_with(
        converter,
        project_root,
        &app.raw_prompt,
        app.conversion_out().as_deref(),
    )
    .await
}

enum BuildThreadMessage {
    Progress(BuildProgress),
    Done(Result<String, String>),
}

/// Runs a real build from the `Build` tab. Deliberately hardcodes
/// `BuildProfile::Dev` (no model-assisted compression, no network call)
/// rather than matching plain `cybersin build`'s `release` default —
/// this trigger has no flags for a user to opt into `release`/`frozen`
/// explicitly, and a landing-screen action silently making network
/// calls would be a bad surprise.
fn run_build_interactive(terminal: &mut TerminalSession, app: &mut App) -> Result<()> {
    let selected_source = match selected_build_source(&app.project_start, app.selected_build_source)
    {
        Ok(source) => source,
        Err(error) => {
            app.build_status = BuildStatus::Failure(error);
            terminal.draw(|frame| render(frame, app))?;
            return Ok(());
        }
    };
    let mut build_log = vec![format!(
        "starting dev build for {}",
        display_project_path(&app.project_start, &selected_source)
    )];
    app.build_status = BuildStatus::Running(build_log.clone());
    terminal.draw(|frame| render(frame, app))?;

    let project_start = app.project_start.clone();
    let log_project_start = app.project_start.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_selected_build_from(project_start, selected_source, |progress| {
            let _ = tx.send(BuildThreadMessage::Progress(progress));
        });
        let _ = tx.send(BuildThreadMessage::Done(result));
    });

    loop {
        while let Ok(message) = rx.try_recv() {
            match message {
                BuildThreadMessage::Progress(progress) => {
                    let line = format_build_progress(&app.project_start, progress);
                    build_log.push(line.clone());
                    push_build_progress_line(app, line);
                }
                BuildThreadMessage::Done(result) => {
                    let final_line = match &result {
                        Ok(message) => format!("success: {message}"),
                        Err(error) => format!("failure: {error}"),
                    };
                    build_log.push(final_line.clone());
                    if let Err(error) = write_build_log(&log_project_start, &build_log) {
                        build_log.push(format!("warning: failed to write build log: {error}"));
                    }
                    app.build_status = match result {
                        Ok(message) => {
                            app.ops_status = OpsStatus::Idle;
                            app.selected_ops_entry = 0;
                            BuildStatus::Success(format!("{message}\nlogged to dist/build.log"))
                        }
                        Err(error) => BuildStatus::Failure(error),
                    };
                    terminal.draw(|frame| render(frame, app))?;
                    return Ok(());
                }
            }
        }

        terminal.draw(|frame| render(frame, app))?;
        if event::poll(Duration::from_millis(50)).context("polling terminal input")? {
            if let Event::Key(key) = event::read().context("reading terminal input")? {
                app.handle_key(key);
            }
        }
    }
}

#[cfg(test)]
fn run_build_from(
    project_start: PathBuf,
    on_progress: impl FnMut(BuildProgress),
) -> Result<String, String> {
    let project_root = resolve_project_root(&project_start)?;
    build::run_into_with_progress(
        &project_root,
        &project_root.join("dist"),
        BuildProfile::Dev,
        false,
        None,
        on_progress,
    )
    .map(|message| message.unwrap_or_else(|| "build complete".to_string()))
}

fn run_selected_build_from(
    project_start: PathBuf,
    source: PathBuf,
    on_progress: impl FnMut(BuildProgress),
) -> Result<String, String> {
    let project_root = resolve_project_root(&project_start)?;
    let registry = registry();
    let spec = registry
        .get(SCAFFOLD_BUILD_WORKFLOW_ID)
        .ok_or_else(|| "scaffold/build workflow capability is not registered".to_string())?;
    let mut events = tui_safety_events(spec)?;
    let execution = execute_scaffold_build_with_progress(
        ScaffoldBuildInput::new(&project_root, &source, BuildProfile::Dev, false),
        on_progress,
    );
    events.extend(execution.events);
    build_summary(&events).unwrap_or_else(|| {
        Err("cybersin build failed: workflow did not emit a terminal event".to_string())
    })
}

fn push_build_progress_line(app: &mut App, line: String) {
    let BuildStatus::Running(lines) = &mut app.build_status else {
        return;
    };
    lines.push(line);
    const MAX_PROGRESS_LINES: usize = 12;
    if lines.len() > MAX_PROGRESS_LINES {
        let overflow = lines.len() - MAX_PROGRESS_LINES;
        lines.drain(0..overflow);
    }
}

fn selected_build_source(project_start: &Path, selected_index: usize) -> Result<PathBuf, String> {
    let sources = build_sources(project_start)?;
    sources
        .get(selected_index.min(sources.len().saturating_sub(1)))
        .cloned()
        .ok_or_else(|| "error: no *.prompt.yaml sources found".to_string())
}

fn build_sources(project_start: &Path) -> Result<Vec<PathBuf>, String> {
    let root = resolve_project_root(project_start)?;
    cybersin_frontend::discover_prompt_sources(&root)
        .map_err(|error| format!("error: failed to discover prompts: {error}"))
}

fn write_build_log(project_start: &Path, lines: &[String]) -> Result<(), String> {
    let root = resolve_project_root(project_start)?;
    let dist = root.join("dist");
    fs::create_dir_all(&dist)
        .map_err(|e| format!("error: failed to create {}: {e}", dist.display()))?;
    let mut text = lines.join("\n");
    text.push('\n');
    fs::write(dist.join("build.log"), text).map_err(|e| {
        format!(
            "error: failed to write {}: {e}",
            dist.join("build.log").display()
        )
    })
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enabling raw terminal mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("entering alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("creating terminal backend")?;
        Ok(Self { terminal })
    }

    fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.terminal.draw(f)?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
    }
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);

    let title = match app.screen {
        Screen::Home => "Cybersin".to_string(),
        Screen::Workspace => format!("Cybersin / {}", app.workspace_tab.title()),
        Screen::CapabilityBrowser => "Cybersin / Capabilities".to_string(),
    };
    frame.render_widget(
        Paragraph::new(title)
            .block(Block::default().borders(Borders::ALL))
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[0],
    );

    match app.screen {
        Screen::Home => render_home(frame, app, chunks[1]),
        Screen::Workspace => render_workspace(frame, app, chunks[1]),
        Screen::CapabilityBrowser => render_capability_browser(frame, app, chunks[1]),
    }

    frame.render_widget(Paragraph::new(footer_text(app)), chunks[2]);
    if app.show_help {
        render_help(frame, area);
    }
}

/// Block-letter "CYBERSIN" wordmark drawn above the landing hint panel.
const CYBERSIN_TITLE: &str = concat!(
    " ###  #   # ####  ##### ####   #### ##### #   #\n",
    "#      # #  #   # #     #   # #       #   ##  #\n",
    "#       #   ####  ####  ####   ###    #   # # #\n",
    "#       #   #   # #     #  #      #   #   #  ##\n",
    " ###    #   ####  ##### #   # ####  ##### #   #",
);

fn render_home(frame: &mut Frame, _app: &App, area: Rect) {
    render_control_room_backdrop(frame, area);

    let title_lines: Vec<&str> = CYBERSIN_TITLE.lines().collect();
    let title_height = title_lines.len() as u16;
    let title_width = title_lines.iter().map(|line| line.len()).max().unwrap_or(0) as u16;

    let gap = 1;
    let panel_width = 46;
    let panel_height = 3;
    let block_width = title_width.max(panel_width);
    let block_height = title_height + gap + panel_height;

    let block = centered_rect(area, block_width, block_height);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(title_height),
            Constraint::Length(gap),
            Constraint::Length(panel_height),
        ])
        .split(block);

    let title_rect = centered_rect(rows[0], title_width, title_height);
    frame.render_widget(Clear, pad_rect(title_rect, 1, area));
    frame.render_widget(
        Paragraph::new(CYBERSIN_TITLE)
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .bg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
        title_rect,
    );

    let panel = centered_rect(rows[2], panel_width, panel_height);
    frame.render_widget(Clear, pad_rect(panel, 1, area));
    frame.render_widget(
        Paragraph::new("Enter converts a prompt · b browses capabilities")
            .alignment(Alignment::Center)
            .style(Style::default().bg(Color::Black))
            .block(focused_block(" Cybersin ", true)),
        panel,
    );
}

fn render_capability_browser(frame: &mut Frame, app: &App, area: Rect) {
    let registry = registry();
    let specs = registry.specs();
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    let selected = app.selected_capability.min(specs.len().saturating_sub(1));
    let items = specs.iter().map(|spec| {
        ListItem::new(format!(
            "{:<24} {:<10} {:<18} {}",
            spec.title,
            category_label(&spec.category),
            availability_label(&spec.adapters.tui),
            safety_label(&spec.safety),
        ))
    });
    let mut state = ListState::default().with_selected((!specs.is_empty()).then_some(selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Capabilities "),
            )
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(Color::Yellow)),
        chunks[0],
        &mut state,
    );

    let detail = specs
        .get(selected)
        .map(|spec| capability_detail_text(spec, app))
        .unwrap_or_else(|| "No capabilities registered.".to_string());
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title(" Detail "))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
}

fn capability_detail_text(spec: &CapabilitySpec, app: &App) -> String {
    let action = match &spec.adapters.tui {
        AdapterSupport::Generic => "Enter/Ctrl+R/F5 submits the form.",
        AdapterSupport::Unavailable { reason } => reason,
        AdapterSupport::Available => "This capability is CLI-available, not generic TUI-ready.",
        AdapterSupport::Custom => "This capability has a custom TUI surface.",
    };
    let form = capability_form_text(&app.capability_form, app.focus == Focus::CapabilityForm);
    format!(
        "{}\n{}\n\nid: {}\ncategory: {}\nsafety: {}\navailability: {}\noutputs: {}\n\n{}\n\nInput\n{}\n\nStatus\n{}",
        spec.title,
        spec.summary,
        spec.id.as_str(),
        category_label(&spec.category),
        safety_label(&spec.safety),
        availability_label(&spec.adapters.tui),
        spec.output_modes
            .iter()
            .map(|mode| format!("{mode:?}"))
            .collect::<Vec<_>>()
            .join(", "),
        action,
        form,
        capability_status_text(&app.capability_status),
    )
}

fn capability_form_text(form: &CapabilityFormState, focused: bool) -> String {
    if form.fields.is_empty() {
        return "(no input fields)".to_string();
    }
    form.fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let cursor = if focused && index == form.selected_field {
                ">"
            } else {
                " "
            };
            let required = if field.required { "*" } else { " " };
            let value = capability_form_value_text(field);
            let hint = if field.description.is_empty() {
                String::new()
            } else {
                format!(" - {}", field.description)
            };
            format!("{cursor} {required} {:<24} {value}{hint}", field.name)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn capability_form_value_text(field: &CapabilityFormField) -> String {
    match (&field.kind, &field.value) {
        (CapabilityFormFieldKind::String { nullable }, CapabilityFormValue::Text(value)) => {
            if value.is_empty() {
                if *nullable {
                    "<null>".to_string()
                } else {
                    "<empty>".to_string()
                }
            } else {
                value.clone()
            }
        }
        (CapabilityFormFieldKind::Boolean, CapabilityFormValue::Boolean(value)) => {
            if *value {
                "[x]".to_string()
            } else {
                "[ ]".to_string()
            }
        }
        (CapabilityFormFieldKind::Enum { values }, CapabilityFormValue::Enum(index)) => values
            .get(*index)
            .cloned()
            .unwrap_or_else(|| "<empty>".to_string()),
        _ => "<unsupported>".to_string(),
    }
}

fn capability_status_text(status: &CapabilityStatus) -> String {
    match status {
        CapabilityStatus::Idle => "No capability run yet.".to_string(),
        CapabilityStatus::Running(message) => message.clone(),
        CapabilityStatus::Success(output) => output.clone(),
        CapabilityStatus::Failure(error) => format!("error: {error}"),
    }
}

fn category_label(category: &crate::capabilities::CapabilityCategory) -> &'static str {
    match category {
        crate::capabilities::CapabilityCategory::Compile => "compile",
        crate::capabilities::CapabilityCategory::Runtime => "runtime",
        crate::capabilities::CapabilityCategory::Inspection => "inspect",
        crate::capabilities::CapabilityCategory::Control => "control",
        crate::capabilities::CapabilityCategory::Sandbox => "sandbox",
        crate::capabilities::CapabilityCategory::Workflow => "workflow",
    }
}

fn availability_label(support: &AdapterSupport) -> &'static str {
    match support {
        AdapterSupport::Available => "available",
        AdapterSupport::Generic => "generic TUI",
        AdapterSupport::Custom => "custom TUI",
        AdapterSupport::Unavailable { .. } => "not invokable",
    }
}

fn safety_label(safety: &crate::capabilities::SafetyProfile) -> &'static str {
    if matches!(
        safety.confirmation,
        crate::capabilities::ConfirmationPolicy::Required { .. }
    ) {
        "confirmation required"
    } else if safety.file_mutation == crate::capabilities::MutationLevel::Destructive {
        "destructive"
    } else if safety.file_mutation != crate::capabilities::MutationLevel::None {
        "writes project"
    } else if safety.runtime_state_mutation != crate::capabilities::MutationLevel::None {
        "writes runtime"
    } else {
        "read only"
    }
}

/// A procedurally generated wall of glowing "monitors" behind the
/// landing hint panel — an original, terminal-native stand-in for
/// `assets/pixel-art-hacker-computer-control-room-*.jpg` (a watermarked
/// stock preview, not a licensed asset, so not something to embed
/// directly). Generated from `(x, y)` rather than a fixed string so it
/// fills whatever size terminal it's drawn into instead of clipping or
/// leaving a ragged edge.
fn render_control_room_backdrop(frame: &mut Frame, area: Rect) {
    let lines: Vec<Line<'static>> = (0..area.height)
        .map(|y| {
            let spans: Vec<Span<'static>> = (0..area.width)
                .map(|x| {
                    let (ch, color) = backdrop_cell(x, y);
                    Span::styled(ch.to_string(), Style::default().fg(color).bg(Color::Black))
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Black)),
        area,
    );
}

/// One character of the control-room backdrop at column `x`, row `y`:
/// tiles `MONITOR_WIDTH`x`MONITOR_HEIGHT` boxes across the whole area,
/// separated by blank wall gaps, cycling a neon blue/cyan/pink palette
/// per monitor so it reads as a wall of independently lit screens
/// rather than one flat color.
fn backdrop_cell(x: u16, y: u16) -> (char, Color) {
    const MONITOR_WIDTH: u16 = 7;
    const MONITOR_HEIGHT: u16 = 3;
    const GAP: u16 = 2;
    const COL_PERIOD: u16 = MONITOR_WIDTH + GAP;
    const ROW_PERIOD: u16 = MONITOR_HEIGHT + GAP;

    let cx = x % COL_PERIOD;
    let cy = y % ROW_PERIOD;
    if cx >= MONITOR_WIDTH || cy >= MONITOR_HEIGHT {
        return (' ', Color::DarkGray);
    }

    let monitor_index = (x / COL_PERIOD) + (y / ROW_PERIOD);
    let color = match monitor_index % 6 {
        0 => Color::Blue,
        1 => Color::Cyan,
        2 => Color::LightBlue,
        3 => Color::Magenta,
        4 => Color::Blue,
        _ => Color::LightCyan,
    };

    let top = cy == 0;
    let bottom = cy == MONITOR_HEIGHT - 1;
    let left = cx == 0;
    let right = cx == MONITOR_WIDTH - 1;
    let ch = match (top, bottom, left, right) {
        (true, _, true, _) => '┌',
        (true, _, _, true) => '┐',
        (_, true, true, _) => '└',
        (_, true, _, true) => '┘',
        (true, _, _, _) | (_, true, _, _) => '─',
        (_, _, true, _) | (_, _, _, true) => '│',
        _ => {
            const GLYPHS: [char; 4] = ['▓', '▒', '░', '▚'];
            GLYPHS[monitor_index as usize % GLYPHS.len()]
        }
    };
    (ch, color)
}

/// A `width`x`height` rect centered inside `area`, clamped to fit —
/// the standard Ratatui "floating panel over a full-screen backdrop"
/// layout, also used to keep the landing hint panel a fixed, readable
/// size regardless of how large the backdrop around it is.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let height = height.min(area.height);
    let width = width.min(area.width);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(vertical[1]);
    horizontal[1]
}

/// `rect` expanded by `padding` cells on every side, clamped to `bounds`
/// — a blank "halo" cleared around the landing hint panel so it reads as
/// a floating console separated from the backdrop's monitor grid,
/// rather than the two visually colliding edge-to-edge.
fn pad_rect(rect: Rect, padding: u16, bounds: Rect) -> Rect {
    let x = rect.x.saturating_sub(padding).max(bounds.x);
    let y = rect.y.saturating_sub(padding).max(bounds.y);
    let right = (rect.x + rect.width + padding).min(bounds.x + bounds.width);
    let bottom = (rect.y + rect.height + padding).min(bounds.y + bounds.height);
    Rect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

fn render_workspace(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let titles = WorkspaceTab::ALL.iter().map(|tab| Line::from(tab.title()));
    let tabs = Tabs::new(titles)
        .select(app.workspace_tab.index())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Cybersin \u{00b7} \u{2190}/\u{2192} switch "),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, chunks[0]);

    match app.workspace_tab {
        WorkspaceTab::Convert => render_convert(frame, app, chunks[1]),
        WorkspaceTab::Build => render_build(frame, app, chunks[1]),
        WorkspaceTab::Ops => render_ops(frame, app, chunks[1]),
    }
}

fn render_convert(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(7),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(app.raw_prompt.as_str())
            .block(focused_block(" Raw Prompt ", app.focus == Focus::Prompt))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(app.model.as_str())
            .block(focused_block(" Model ", app.focus == Focus::Model)),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(app.out.as_str()).block(focused_block(
            " Output Path (optional) ",
            app.focus == Focus::Out,
        )),
        chunks[2],
    );
    let action = if app.convert_status == ConversionStatus::Running {
        "Converting..."
    } else {
        "Convert  Ctrl+R / F5"
    };
    frame.render_widget(
        Paragraph::new(action).block(focused_block(" Action ", app.focus == Focus::ConvertAction)),
        chunks[3],
    );
    frame.render_widget(convert_status_widget(&app.convert_status), chunks[4]);
}

fn render_build(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(3),
            Constraint::Min(5),
        ])
        .split(area);

    render_build_sources(frame, app, chunks[0]);

    let action = if matches!(app.build_status, BuildStatus::Running(_)) {
        "Building..."
    } else {
        "Build selected prompt (profile: dev)  Enter / Ctrl+R / F5"
    };
    frame.render_widget(
        Paragraph::new(action).block(Block::default().borders(Borders::ALL).title(" Action ")),
        chunks[1],
    );
    frame.render_widget(build_status_widget(&app.build_status), chunks[2]);
}

fn render_build_sources(frame: &mut Frame, app: &App, area: Rect) {
    match resolve_project_root(&app.project_start) {
        Ok(root) => match build_sources(&app.project_start) {
            Ok(sources) if sources.is_empty() => {
                frame.render_widget(
                    Paragraph::new(format!(
                        "project: {}\n\nprompts/ at {}\n  none found",
                        root.display(),
                        root.join("prompts").display()
                    ))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Select prompt "),
                    )
                    .wrap(Wrap { trim: false }),
                    area,
                );
            }
            Ok(sources) => {
                let selected = app
                    .selected_build_source
                    .min(sources.len().saturating_sub(1));
                let items = sources
                    .iter()
                    .map(|source| ListItem::new(display_project_path(&root, source)));
                let mut state = ListState::default().with_selected(Some(selected));
                frame.render_stateful_widget(
                    List::new(items)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(format!(" Select prompt · project: {} ", root.display())),
                        )
                        .highlight_symbol("> ")
                        .highlight_style(Style::default().fg(Color::Yellow)),
                    area,
                    &mut state,
                );
            }
            Err(error) => {
                frame.render_widget(
                    Paragraph::new(error)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(" Select prompt "),
                        )
                        .wrap(Wrap { trim: false }),
                    area,
                );
            }
        },
        Err(error) => {
            frame.render_widget(
                Paragraph::new(error)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Select prompt "),
                    )
                    .wrap(Wrap { trim: false }),
                area,
            );
        }
    }
}

fn render_ops(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(10)])
        .split(area);
    match &app.ops_status {
        OpsStatus::Idle => {
            frame.render_widget(
                Paragraph::new("Press Enter, Ctrl+R, or F5 to load Ops.")
                    .block(Block::default().borders(Borders::ALL).title(" Ops ")),
                area,
            );
        }
        OpsStatus::Running => {
            frame.render_widget(
                Paragraph::new("Loading Ops...")
                    .block(Block::default().borders(Borders::ALL).title(" Ops ")),
                area,
            );
        }
        OpsStatus::Failure(error) => {
            frame.render_widget(
                Paragraph::new(error.clone())
                    .block(Block::default().borders(Borders::ALL).title(" Ops "))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Red)),
                area,
            );
        }
        OpsStatus::Success(entries) => {
            let selected = app.selected_ops_entry.min(entries.len().saturating_sub(1));
            let items = entries
                .iter()
                .map(|entry| ListItem::new(entry.title.as_str()));
            let mut state =
                ListState::default().with_selected((!entries.is_empty()).then_some(selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(focused_block(" Ops ", app.focus != Focus::OpsBuildsList))
                    .highlight_symbol("> ")
                    .highlight_style(Style::default().fg(Color::Yellow)),
                chunks[0],
                &mut state,
            );
            if entries
                .get(selected)
                .is_some_and(|entry| entry.title == "Builds")
            {
                render_ops_builds_panel(frame, app, chunks[1]);
            } else {
                let detail = entries
                    .get(selected)
                    .map(|entry| entry.body.clone())
                    .unwrap_or_else(|| "No Ops entries available.".to_string());
                let title = entries
                    .get(selected)
                    .map(|entry| format!(" {} ", entry.title))
                    .unwrap_or_else(|| " Detail ".to_string());
                frame.render_widget(
                    Paragraph::new(detail)
                        .block(Block::default().borders(Borders::ALL).title(title))
                        .wrap(Wrap { trim: false }),
                    chunks[1],
                );
            }
        }
    }
}

/// The Ops tab's "Builds" row detail panel — an interactive list of
/// compiled agents (rather than the other rows' plain text body) so a
/// build can be selected and, once focus tabs onto it, run directly
/// from the bare TUI.
fn render_ops_builds_panel(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(area);

    let selected = app
        .selected_ops_build
        .min(app.ops_builds.len().saturating_sub(1));
    let items: Vec<ListItem> = if app.ops_builds.is_empty() {
        vec![ListItem::new("no agents found in agents/**/*.agent.yaml")]
    } else {
        app.ops_builds
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
    let mut state = ListState::default();
    if !app.ops_builds.is_empty() {
        state.select(Some(selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(focused_block(
                " Builds \u{00b7} Tab to select \u{00b7} Enter to run ",
                app.focus == Focus::OpsBuildsList,
            ))
            .highlight_symbol("> ")
            .highlight_style(Style::default().fg(Color::Yellow)),
        rows[0],
        &mut state,
    );

    frame.render_widget(ops_run_status_widget(&app.ops_run_status), rows[1]);
}

fn ops_run_status_widget(status: &OpsRunStatus) -> Paragraph<'static> {
    match status {
        OpsRunStatus::Idle => Paragraph::new("Select a build and press Enter to run it.")
            .block(Block::default().borders(Borders::ALL).title(" Run ")),
        OpsRunStatus::Running => Paragraph::new("Running...")
            .block(Block::default().borders(Borders::ALL).title(" Run ")),
        OpsRunStatus::Success(message) => Paragraph::new(message.clone())
            .block(Block::default().borders(Borders::ALL).title(" Run "))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Green)),
        OpsRunStatus::Failure(error) => Paragraph::new(error.clone())
            .block(Block::default().borders(Borders::ALL).title(" Run "))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Red)),
    }
}

fn focused_block(title: &'static str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(style)
}

fn convert_status_widget(status: &ConversionStatus) -> Paragraph<'static> {
    match status {
        ConversionStatus::Idle => Paragraph::new("Idle")
            .block(Block::default().borders(Borders::ALL).title(" Outcome ")),
        ConversionStatus::Running => Paragraph::new("Running conversion...")
            .block(Block::default().borders(Borders::ALL).title(" Outcome ")),
        ConversionStatus::Failure(error) => Paragraph::new(error.clone())
            .block(Block::default().borders(Borders::ALL).title(" Failure "))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Red)),
        ConversionStatus::Success(summary) => Paragraph::new(format!(
            "wrote {}\nself-validation passed\ninferred inputs: {}\ninferred tools: {}\nunmapped content: {}",
            summary.path.display(),
            summary_list(&summary.inputs),
            summary_list(&summary.tools),
            summary_list(&summary.unmapped_sections)
        ))
        .block(Block::default().borders(Borders::ALL).title(" Success "))
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::Green)),
    }
}

fn build_status_widget(status: &BuildStatus) -> Paragraph<'static> {
    match status {
        BuildStatus::Idle => Paragraph::new("No build run yet this session.")
            .block(Block::default().borders(Borders::ALL).title(" Outcome ")),
        BuildStatus::Running(lines) => Paragraph::new(lines.join("\n"))
            .block(Block::default().borders(Borders::ALL).title(" Outcome ")),
        BuildStatus::Failure(error) => Paragraph::new(error.clone())
            .block(Block::default().borders(Borders::ALL).title(" Failure "))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Red)),
        BuildStatus::Success(message) => Paragraph::new(message.clone())
            .block(Block::default().borders(Borders::ALL).title(" Success "))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Green)),
    }
}

/// `Build` tab's info panel: source prompts the user can attempt to
/// compile. `dist/` is build output, so runtime-facing artifact state
/// belongs on the Ops side instead of here.
#[cfg(test)]
fn build_info_text(project_start: &Path) -> String {
    match resolve_project_root(project_start) {
        Ok(root) => format!(
            "project: {}\n\n{}",
            root.display(),
            prompt_sources_snapshot(&root)
        ),
        Err(error) => error,
    }
}

#[cfg(test)]
fn prompt_sources_snapshot(project_root: &Path) -> String {
    match cybersin_frontend::discover_prompt_sources(project_root) {
        Ok(sources) if sources.is_empty() => {
            format!(
                "prompts/ at {}\n  none found",
                project_root.join("prompts").display()
            )
        }
        Ok(sources) => {
            let mut lines = vec![format!(
                "prompts/ at {}\n  files: {}",
                project_root.join("prompts").display(),
                sources.len()
            )];
            lines.extend(sources.into_iter().map(|source| {
                let relative = source.strip_prefix(project_root).unwrap_or(&source);
                format!("  {}", relative.display())
            }));
            lines.join("\n")
        }
        Err(error) => format!("prompts: error discovering sources: {error}"),
    }
}

/// `Ops` tab's info panel — see this module's doc for why it never
/// touches the daemon.
#[cfg(test)]
fn ops_info_text(project_start: &Path) -> String {
    match resolve_project_root(project_start) {
        Ok(root) => {
            let runtime_state = match ops_snapshot(&root) {
                Ok(snapshot) => snapshot,
                Err(error) => error,
            };
            let dist_state = match dist_snapshot(&root) {
                Ok(snapshot) => snapshot,
                Err(error) => error,
            };
            format!(
                "project: {}\n\nruntime output:\n{}\n\n{}",
                root.display(),
                dist_state,
                runtime_state
            )
        }
        Err(error) => error,
    }
}

#[cfg(test)]
fn ops_build_log_text(project_start: &Path) -> String {
    match resolve_project_root(project_start) {
        Ok(root) => {
            let path = root.join("dist/build.log");
            match fs::read_to_string(&path) {
                Ok(text) => format!("build log: {}\n\n{}", path.display(), tail_lines(&text, 14)),
                Err(_) => {
                    "No TUI build log yet. Select a prompt on Build and press Enter.".to_string()
                }
            }
        }
        Err(error) => error,
    }
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

#[cfg(test)]
fn dist_snapshot(project_root: &Path) -> Result<String, String> {
    let defaults = ProjectDefaults::detect(project_root).map_err(|e| e.to_string())?;
    let dist = defaults.dist_default().map_err(|e| e.to_string())?;
    let manifest_path = dist.join("manifest.json");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("error: reading {}: {e}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("error: invalid {}: {e}", manifest_path.display()))?;
    let git_sha = manifest
        .get("git_sha")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-");
    let build_hash = manifest
        .get("build_hash")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-");
    let artifact_count = manifest
        .get("artifacts")
        .and_then(serde_json::Value::as_object)
        .map_or(0, |artifacts| artifacts.len());
    Ok(format!(
        "dist/ at {}\n  git sha: {}\n  build hash: {}\n  artifacts: {}",
        dist.display(),
        short_hash(git_sha),
        short_hash(build_hash),
        artifact_count
    ))
}

#[cfg(test)]
fn ops_snapshot(project_root: &Path) -> Result<String, String> {
    let defaults = ProjectDefaults::detect(project_root).map_err(|e| e.to_string())?;
    let db_path = defaults.db_default();
    let text = match std::fs::metadata(&db_path) {
        Ok(meta) => {
            let age = meta
                .modified()
                .ok()
                .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
                .map(format_age)
                .unwrap_or_else(|| "unknown".to_string());
            format!("state db: {}\n  last activity: {}", db_path.display(), age)
        }
        Err(_) => format!(
            "state db: {} (not created yet \u{2014} no session has run against this project)",
            db_path.display()
        ),
    };
    Ok(text)
}

fn display_project_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(test)]
fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

fn format_build_progress(project_start: &Path, progress: BuildProgress) -> String {
    let project_root = resolve_project_root(project_start).ok();
    let display_path = |path: PathBuf| {
        project_root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(&path)
            .display()
            .to_string()
    };
    match progress {
        BuildProgress::DiscoveredPrompts(sources) => {
            format!("discovered {} prompt source(s)", sources.len())
        }
        BuildProgress::ClearingDist(path) => format!("clearing {}", display_path(path)),
        BuildProgress::PromptStarted { name, source } => {
            format!("prompt {name}: {}", display_path(source))
        }
        BuildProgress::PassFinished { prompt, pass } => format!("pass {pass}: {prompt}"),
        BuildProgress::PromptWritten(prompt) => format!("wrote prompt artifact: {prompt}"),
        BuildProgress::Routing => "compiled routing.json".to_string(),
        BuildProgress::Cache => "seeded cache.json".to_string(),
        BuildProgress::Tools => "compiled tool policy/assets".to_string(),
        BuildProgress::Manifest => "wrote manifest.json".to_string(),
    }
}

fn footer_text(app: &App) -> String {
    match app.screen {
        Screen::Home => "Enter convert \u{00b7} b capabilities \u{00b7} ? help \u{00b7} q quit".to_string(),
        Screen::CapabilityBrowser => {
            "Tab form/list \u{00b7} \u{2191}/\u{2193} select/field \u{00b7} \u{2190}/\u{2192} cycle \u{00b7} Enter/Ctrl+R/F5 run \u{00b7} Esc back \u{00b7} ? help".to_string()
        }
        Screen::Workspace => match app.workspace_tab {
            WorkspaceTab::Convert => {
                "Ctrl+R/F5 convert \u{00b7} Tab/Shift-Tab focus \u{00b7} \u{2190}/\u{2192} tab \u{00b7} Enter type/act \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string()
            }
            WorkspaceTab::Build => {
                "Ctrl+R/F5 build \u{00b7} \u{2190}/\u{2192} tab \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string()
            }
            WorkspaceTab::Ops if app.focus == Focus::OpsBuildsList => {
                "\u{2191}/\u{2193} select build \u{00b7} Enter run build \u{00b7} Tab back to Ops list \u{00b7} \u{2190}/\u{2192} tab \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string()
            }
            WorkspaceTab::Ops => "Enter/Ctrl+R/F5 refresh \u{00b7} \u{2191}/\u{2193} select \u{00b7} Tab to Builds list \u{00b7} \u{2190}/\u{2192} tab \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string(),
        },
    }
}

fn render_help(frame: &mut Frame, area: Rect) {
    let width = area.width.min(74);
    let height = area.height.min(11);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(
            "Ctrl+R or F5 runs the active tab's primary action\nb opens the capability browser from Home\n\u{2190}/\u{2192} switches Convert/Build/Ops tabs\n\u{2191}/\u{2193} selects rows in Build/Ops/capabilities\nTab / Shift-Tab moves focus inside Convert, or, on Ops's Builds row, into its build list\nEnter starts converting from Home, runs the focused Convert action, builds, refreshes Ops, runs the selected Ops build, or invokes a generic capability\nEsc goes back or dismisses overlays\nq quits when focus is outside the prompt editor\n-help, -h, and --help print CLI help",
        )
        .block(Block::default().borders(Borders::ALL).title(" Help "))
        .wrap(Wrap { trim: false }),
        rect,
    );
}

fn summary_list(items: &[String]) -> String {
    if items.is_empty() {
        "none".to_string()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    struct FakeConverter;

    #[async_trait]
    impl PromptConversionModel for FakeConverter {
        async fn convert(
            &self,
            _raw_prompt: &str,
            _schema: &serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(json!({
                "name": "draft",
                "quality": "medium",
                "inputs": [],
                "tools": [],
                "sections": [{"id": "prompt", "priority": 100, "body": "ignored"}],
                "output_contract": null
            }))
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn write_hello_prompt(project: &Path) {
        std::fs::write(
            project.join("prompts/hello.prompt.yaml"),
            "name: hello\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Hello.\n",
        )
        .unwrap();
    }

    #[test]
    fn home_opens_conversion_and_back_preserves_draft() {
        let mut app = App::default();

        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('H')));
        app.handle_key(key(KeyCode::Char('i')));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Workspace);
        assert_eq!(app.workspace_tab, WorkspaceTab::Convert);
        assert_eq!(app.raw_prompt, "Hi");
    }

    #[test]
    fn enter_from_home_always_opens_convert() {
        let mut app = App::default();
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Workspace);
        assert_eq!(app.workspace_tab, WorkspaceTab::Convert);
    }

    #[test]
    fn home_opens_capability_browser_without_disturbing_convert_entry() {
        let mut app = App::default();

        app.handle_key(key(KeyCode::Char('b')));

        assert_eq!(app.screen, Screen::CapabilityBrowser);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Home);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Workspace);
        assert_eq!(app.workspace_tab, WorkspaceTab::Convert);
    }

    #[test]
    fn capability_browser_moves_selection_and_requests_execution() {
        let mut app = App::default();
        app.enter_capability_browser();

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_capability, 1);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected_capability, 0);

        let action = app.handle_key(key(KeyCode::Enter));
        assert!(matches!(action, AppAction::ExecuteCapability));
    }

    #[test]
    fn capability_browser_tabs_into_schema_form_and_edits_state() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let mut app = App::new(project.path().to_path_buf());
        app.enter_capability_browser();
        app.selected_capability = registry()
            .specs()
            .iter()
            .position(|spec| spec.id.as_str() == BUILD_CAPABILITY_ID)
            .unwrap();
        app.capability_form
            .sync_selection(&app.project_start, app.selected_capability);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::CapabilityForm);
        assert_eq!(app.capability_form.fields[0].name, "frozen");

        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            app.capability_form.fields[0].value,
            CapabilityFormValue::Boolean(true)
        );

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.capability_form.fields[1].name, "profile");
        app.handle_key(key(KeyCode::Right));
        assert_eq!(
            app.capability_form.fields[1].value,
            CapabilityFormValue::Enum(1)
        );

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.capability_form.fields[2].name, "project_path");
        let default_project_path = capability_form_value_text(&app.capability_form.fields[2]);
        assert_eq!(default_project_path, project.path().display().to_string());

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Navigation);
    }

    #[test]
    fn left_right_cycle_workspace_tabs_and_wrap() {
        let mut app = App::default();
        app.enter_convert();
        assert_eq!(app.workspace_tab, WorkspaceTab::Convert);

        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.workspace_tab, WorkspaceTab::Build);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.workspace_tab, WorkspaceTab::Ops);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.workspace_tab, WorkspaceTab::Convert);

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.workspace_tab, WorkspaceTab::Ops);
    }

    #[test]
    fn conversion_focus_cycles_through_fields() {
        let mut app = App::default();
        app.enter_convert();

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Model);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Out);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::ConvertAction);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn q_does_not_quit_inside_prompt_editor() {
        let mut app = App::default();
        app.enter_convert();

        app.handle_key(key(KeyCode::Char('q')));

        assert!(!app.should_quit);
        assert_eq!(app.raw_prompt, "q");
    }

    #[test]
    fn ctrl_r_converts_from_the_prompt_editor() {
        let mut app = App::default();
        app.enter_convert();
        app.raw_prompt = "Turn this into a prompt source.".to_string();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::Convert));
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn empty_prompt_conversion_is_rejected_before_model_call() {
        let mut app = App::default();
        app.enter_convert();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::None));
        assert_eq!(
            app.convert_status,
            ConversionStatus::Failure("Enter a prompt before converting.".to_string())
        );
    }

    #[test]
    fn build_tab_ctrl_r_requests_build_without_validation() {
        let mut app = App::default();
        app.enter_convert();
        app.switch_tab(WorkspaceTab::Build);

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::Build));
    }

    #[test]
    fn build_tab_enter_requests_build_and_up_down_select_sources() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        write_hello_prompt(project.path());
        std::fs::write(
            project.path().join("prompts/second.prompt.yaml"),
            "name: second\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Build me.\n",
        )
        .unwrap();
        let mut app = App::new(project.path().to_path_buf());
        app.enter_convert();
        app.switch_tab(WorkspaceTab::Build);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_build_source, 1);
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_build_source, 1);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.selected_build_source, 0);

        let action = app.handle_key(key(KeyCode::Enter));

        assert!(matches!(action, AppAction::Build));
    }

    #[test]
    fn selected_build_uses_scaffold_build_workflow() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        write_hello_prompt(project.path());
        let source = project.path().join("prompts/hello.prompt.yaml");
        let mut progress = Vec::new();

        let message = run_selected_build_from(project.path().to_path_buf(), source, |event| {
            progress.push(event)
        })
        .unwrap();

        assert!(message.contains("agent agents/hello.agent.yaml"));
        assert!(project.path().join("agents/hello.agent.yaml").exists());
        assert!(project.path().join("harnesses/hello.script.yaml").exists());
        assert!(project.path().join("dist/prompts/hello.json").exists());
        assert!(progress.iter().any(
            |event| matches!(event, BuildProgress::PromptStarted { name, .. } if name == "hello")
        ));
    }

    #[test]
    fn ops_tab_primary_action_refreshes_ops() {
        let mut app = App::default();
        app.enter_convert();
        app.switch_tab(WorkspaceTab::Ops);

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::RefreshOps));
    }

    /// The flow this feature is for: land on Ops, arrow down to the
    /// "Builds" row, Tab over into the adjacent Builds list, arrow down
    /// to a build, then Enter to run it.
    #[test]
    fn ops_builds_row_tabs_into_an_adjacent_list_and_enter_runs_the_selection() {
        let mut app = App::default();
        app.enter_convert();
        app.switch_tab(WorkspaceTab::Ops);
        app.ops_status = OpsStatus::Success(vec![
            OpsEntry {
                title: "dist/build.log".to_string(),
                body: "log body".to_string(),
            },
            OpsEntry {
                title: "Builds".to_string(),
                body: "Builds (2)".to_string(),
            },
        ]);
        app.ops_builds = vec![
            ops::OpsBuild {
                name: "first-agent".to_string(),
                path: PathBuf::from("agents/first.agent.yaml"),
                build_hash_short: Some("abc123".to_string()),
            },
            ops::OpsBuild {
                name: "second-agent".to_string(),
                path: PathBuf::from("agents/second.agent.yaml"),
                build_hash_short: None,
            },
        ];

        // Tab does nothing while a non-Builds row is selected.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Navigation);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_ops_entry, 1);

        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::OpsBuildsList);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.selected_ops_build, 1);

        let action = app.handle_key(key(KeyCode::Enter));
        match action {
            AppAction::RunOpsBuild(path) => {
                assert_eq!(path, PathBuf::from("agents/second.agent.yaml"));
            }
            other => panic!("expected AppAction::RunOpsBuild, got {other:?}"),
        }

        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Navigation);
    }

    #[tokio::test]
    async fn conversion_uses_literal_editor_text_and_fake_model() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
        let mut app = App::default();
        app.raw_prompt = "Summarize /tmp/looks-like-a-path.\nKeep it short.".to_string();
        app.out = "drafts/from-tui.prompt.yaml".to_string();

        let report = run_conversion_with_model(&FakeConverter, project.path(), &app)
            .await
            .unwrap();

        assert_eq!(
            report.path,
            project.path().join("drafts/from-tui.prompt.yaml")
        );
        assert!(report.inputs.is_empty());
        assert!(std::fs::read_to_string(report.path)
            .unwrap()
            .contains("Summarize /tmp/looks-like-a-path."));
    }

    #[tokio::test]
    async fn conversion_finds_a_single_descendant_project_from_repo_root_like_cwd() {
        let repo = tempfile::tempdir().unwrap();
        let project = repo.path().join("fixtures/ic1-research-team");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("cybersin.yaml"), "name: test\n").unwrap();

        let mut app = App::new(repo.path().to_path_buf());
        app.raw_prompt = "Convert this from the repository root.".to_string();

        let report = run_conversion_with_model(&FakeConverter, &app.project_start, &app)
            .await
            .unwrap();

        assert_eq!(app.project_start, project);
        assert_eq!(
            report.path,
            app.project_start.join("prompts/draft.prompt.yaml")
        );
        assert!(std::fs::read_to_string(report.path)
            .unwrap()
            .contains("Convert this from the repository root."));
    }

    #[test]
    fn dist_snapshot_reports_not_built_when_dist_missing() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
        std::fs::create_dir_all(project.path().join("prompts")).unwrap();
        std::fs::write(
            project.path().join("prompts/hello.prompt.yaml"),
            "name: hello\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Hello.\n",
        )
        .unwrap();

        let error = dist_snapshot(project.path()).unwrap_err();

        assert!(error.contains("no dist/ found"));

        let info = build_info_text(project.path());
        assert!(info.contains("prompts/ at"));
        assert!(info.contains("files: 1"));
        assert!(info.contains("prompts/hello.prompt.yaml"));
        assert!(!info.contains("no dist/ found"));
    }

    #[test]
    fn info_panels_resolve_from_the_app_project_anchor() {
        let outside_project = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
        let nested = project.path().join("nested/path");
        std::fs::create_dir_all(&nested).unwrap();

        let outside_text = build_info_text(outside_project.path());
        assert!(outside_text.contains("cybersin.yaml"));

        let anchored_text = build_info_text(&nested);
        assert!(anchored_text.contains(&format!("project: {}", project.path().display())));
        assert!(anchored_text.contains("prompts/ at"));

        let ops_text = ops_info_text(&nested);
        assert!(ops_text.contains(&format!("project: {}", project.path().display())));
        assert!(ops_text.contains("runtime output:"));
        assert!(ops_text.contains("no dist/ found"));
        assert!(ops_text.contains("state db:"));
    }

    #[test]
    fn dist_snapshot_reads_manifest_when_built() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();
        let dist = project.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(
            dist.join("manifest.json"),
            r#"{"schema_version":1,"build_hash":"abcdefabcdefabcdef","git_sha":"1234567890","artifacts":{"a.json":"x","b.json":"y"}}"#,
        )
        .unwrap();

        let text = dist_snapshot(project.path()).unwrap();

        assert!(text.contains("artifacts: 2"));
    }

    #[test]
    fn build_tab_progress_lists_prompt_passes() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        write_hello_prompt(project.path());
        let mut lines = Vec::new();

        let result = run_build_from(project.path().to_path_buf(), |progress| {
            lines.push(format_build_progress(project.path(), progress));
        })
        .unwrap();

        assert!(result.contains("built"));
        assert!(lines.iter().any(|line| line.contains("prompt hello")));
        assert!(lines.iter().any(|line| line.contains("pass lint-fast")));
        assert!(lines.iter().any(|line| line.contains("pass budget")));
        assert!(lines
            .iter()
            .any(|line| line.contains("wrote manifest.json")));
    }

    #[test]
    fn selected_build_compiles_that_prompt_and_ops_reads_dist_build_log() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let selected = project.path().join("prompts/second.prompt.yaml");
        std::fs::write(
            &selected,
            "name: second\nquality: medium\nsections:\n- id: prompt\n  priority: 100\n  body: Build me.\n",
        )
        .unwrap();
        let mut lines = vec![format!(
            "starting dev build for {}",
            display_project_path(project.path(), &selected)
        )];

        let result = run_selected_build_from(project.path().to_path_buf(), selected, |progress| {
            lines.push(format_build_progress(project.path(), progress));
        })
        .unwrap();
        lines.push(format!("success: {result}"));
        write_build_log(project.path(), &lines).unwrap();

        assert!(project.path().join("dist/prompts/second.json").exists());
        assert!(!project.path().join("dist/prompts/hello.json").exists());
        let agent_text =
            std::fs::read_to_string(project.path().join("agents/second.agent.yaml")).unwrap();
        assert!(agent_text.contains("[\"scripted_harness\", \"harnesses/second.script.yaml\"]"));
        let script_text =
            std::fs::read_to_string(project.path().join("harnesses/second.script.yaml")).unwrap();
        assert!(script_text.contains("prompt_name: \"second\""));
        let ops_text = ops_build_log_text(project.path());
        assert!(ops_text.contains("dist/build.log"));
        assert!(ops_text.contains("prompt second"));
        assert!(ops_text.contains("success: built"));
        assert!(result.contains("agents/second.agent.yaml"));
    }

    #[tokio::test]
    async fn selected_build_creates_prompt_agent_visible_to_ops() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let selected = project
            .path()
            .join("prompts/bismarck_germany_unification_strategy.prompt.yaml");
        std::fs::write(
            &selected,
            "name: bismarck_germany_unification_strategy\nquality: medium\nsections:\n- id: strategy\n  priority: 100\n  body: Explain Otto Von Bismarck's strategy in unifying Germany.\n",
        )
        .unwrap();

        run_selected_build_from(project.path().to_path_buf(), selected, |_| {}).unwrap();

        let agent_path = project
            .path()
            .join("agents/bismarck-germany-unification-strategy.agent.yaml");
        let agent_text = std::fs::read_to_string(&agent_path).unwrap();
        assert!(agent_text.contains("name: bismarck-germany-unification-strategy-agent"));
        assert!(agent_text.contains(
            "[\"scripted_harness\", \"harnesses/bismarck-germany-unification-strategy.script.yaml\"]"
        ));
        let script_text = std::fs::read_to_string(
            project
                .path()
                .join("harnesses/bismarck-germany-unification-strategy.script.yaml"),
        )
        .unwrap();
        assert!(script_text.contains("prompt_name: \"bismarck_germany_unification_strategy\""));

        let entries = load_ops_entries(project.path()).await.unwrap();
        let builds = entries
            .iter()
            .find(|entry| entry.title == "Builds")
            .expect("builds entry");
        assert!(builds
            .body
            .contains("bismarck-germany-unification-strategy-agent"));
    }

    #[test]
    fn ops_snapshot_reports_no_db_before_any_run() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();

        let text = ops_snapshot(project.path()).unwrap();

        assert!(text.contains("not created yet"));
    }

    #[test]
    fn render_home_and_small_conversion_layouts() {
        let backend = TestBackend::new(48, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();

        terminal.draw(|frame| render(frame, &app)).unwrap();
        app.enter_convert();
        app.raw_prompt = "Line one\nLine two".to_string();
        app.convert_status = ConversionStatus::Failure("network failed".to_string());
        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Raw Prompt"));
        assert!(rendered.contains("network failed"));
    }

    #[test]
    fn home_backdrop_surrounds_the_hint_panel_without_hiding_it() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Enter converts a prompt"));
        assert!(rendered.contains('┌'));
    }

    #[test]
    fn home_backdrop_does_not_panic_on_a_tiny_terminal() {
        let backend = TestBackend::new(6, 4);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();

        terminal.draw(|frame| render(frame, &app)).unwrap();
    }

    #[test]
    fn render_workspace_shows_all_tab_titles() {
        let backend = TestBackend::new(48, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.enter_convert();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Convert"));
        assert!(rendered.contains("Build"));
        assert!(rendered.contains("Ops"));
    }

    #[test]
    fn render_capability_browser_lists_catalog_metadata() {
        let backend = TestBackend::new(110, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.enter_capability_browser();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Capabilities"));
        assert!(rendered.contains("Build project"));
        assert!(rendered.contains("Check prompt sources"));
        assert!(rendered.contains("List traces"));
        assert!(rendered.contains("generic TUI"));
        assert!(rendered.contains("not invokable"));
        assert!(rendered.contains("writes project"));
    }

    #[test]
    fn render_capability_browser_shows_compact_schema_form() {
        let backend = TestBackend::new(180, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let mut app = App::new(project.path().to_path_buf());
        app.enter_capability_browser();
        app.selected_capability = registry()
            .specs()
            .iter()
            .position(|spec| spec.id.as_str() == BUILD_CAPABILITY_ID)
            .unwrap();
        app.capability_form
            .sync_selection(&app.project_start, app.selected_capability);

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("Input"));
        assert!(rendered.contains("* project_path"));
        assert!(rendered.contains("* profile"));
        assert!(rendered.contains("* frozen"));
        assert!(rendered.contains("selected_prompt_source"));
    }

    #[test]
    fn normalized_capability_input_matches_build_check_and_trace_shapes() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let registry = registry();

        let build = registry.get(BUILD_CAPABILITY_ID).unwrap();
        let build_form = CapabilityFormState::from_schema(project.path(), build);
        let build_input = normalize_capability_input(project.path(), build, &build_form).unwrap();
        assert_eq!(
            build_input,
            json!({
                "project_path": project.path().display().to_string(),
                "profile": "dev",
                "frozen": false,
                "selected_prompt_source": null
            })
        );

        let check = registry.get(CHECK_CAPABILITY_ID).unwrap();
        let mut check_form = CapabilityFormState::from_schema(project.path(), check);
        let check_input = normalize_capability_input(project.path(), check, &check_form).unwrap();
        assert_eq!(
            check_input,
            json!({
                "path": project.path().display().to_string()
            })
        );
        if let CapabilityFormValue::Text(value) = &mut check_form.fields[0].value {
            value.clear();
        }
        assert_eq!(
            normalize_capability_input(project.path(), check, &check_form).unwrap_err(),
            "path is required"
        );

        let trace = registry.get(TRACE_LS_CAPABILITY_ID).unwrap();
        let mut trace_form = CapabilityFormState::from_schema(project.path(), trace);
        let limit = trace_form
            .fields
            .iter_mut()
            .find(|field| field.name == "limit")
            .expect("limit field");
        if let CapabilityFormValue::Text(value) = &mut limit.value {
            *value = "50".to_string();
        }
        let trace_input = normalize_capability_input(project.path(), trace, &trace_form).unwrap();
        assert_eq!(
            trace_input,
            json!({
                "session": null,
                "agent": null,
                "model": null,
                "limit": "50"
            })
        );
        let invalid_limit = json!({ "limit": "many" });
        assert_eq!(
            optional_u32(&invalid_limit, "limit").unwrap_err(),
            "limit must be a non-negative integer"
        );
    }

    #[tokio::test]
    async fn generic_browser_runs_check_capability_and_reports_unavailable_entries() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        write_hello_prompt(project.path());
        let mut app = App::new(project.path().to_path_buf());
        app.enter_capability_browser();
        let registry = registry();
        app.selected_capability = registry
            .specs()
            .iter()
            .position(|spec| spec.id.as_str() == CHECK_CAPABILITY_ID)
            .unwrap();

        let check = selected_capability_spec(&app).unwrap();
        let output = run_generic_capability(&app.project_start, &check, &app.capability_form)
            .await
            .unwrap();

        assert!(output.contains("started compile.check"));
        assert!(output.contains("ok"));
        assert!(output.contains("completed"));

        app.selected_capability = registry
            .specs()
            .iter()
            .position(|spec| spec.id.as_str() == "compile.fmt")
            .unwrap();
        app.capability_form
            .sync_selection(&app.project_start, app.selected_capability);
        let build = selected_capability_spec(&app).unwrap();

        let error = run_generic_capability(&app.project_start, &build, &app.capability_form)
            .await
            .unwrap_err();

        assert!(error.contains("not wired into the TUI adapter yet"));
    }

    #[tokio::test]
    async fn generic_browser_runs_trace_ls_capability() {
        let project = tempfile::tempdir().unwrap();
        crate::commands::init::run(project.path()).unwrap();
        let mut app = App::new(project.path().to_path_buf());
        app.enter_capability_browser();
        app.selected_capability = registry()
            .specs()
            .iter()
            .position(|spec| spec.id.as_str() == TRACE_LS_CAPABILITY_ID)
            .unwrap();

        let trace_ls = selected_capability_spec(&app).unwrap();
        let output = run_generic_capability(&app.project_start, &trace_ls, &app.capability_form)
            .await
            .unwrap();

        assert!(output.contains("started inspection.trace.ls"));
        assert!(output.contains("no spans recorded yet"));
        assert!(output.contains("completed"));
    }

    #[test]
    fn render_ops_shows_selectable_list_without_project_state_panel() {
        let backend = TestBackend::new(72, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.enter_convert();
        app.switch_tab(WorkspaceTab::Ops);
        app.ops_status = OpsStatus::Success(vec![
            OpsEntry {
                title: "dist/build.log".to_string(),
                body: "success: built".to_string(),
            },
            OpsEntry {
                title: "Sessions".to_string(),
                body: "Sessions (0)".to_string(),
            },
        ]);

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("dist/build.log"));
        assert!(rendered.contains("success: built"));
        assert!(!rendered.contains("Project state"));
    }

    #[test]
    fn render_ops_shows_an_interactive_builds_list_when_builds_row_selected() {
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        app.enter_convert();
        app.switch_tab(WorkspaceTab::Ops);
        app.ops_status = OpsStatus::Success(vec![
            OpsEntry {
                title: "dist/build.log".to_string(),
                body: "success: built".to_string(),
            },
            OpsEntry {
                title: "Builds".to_string(),
                body: "Builds (2)".to_string(),
            },
        ]);
        app.selected_ops_entry = 1;
        app.ops_builds = vec![
            ops::OpsBuild {
                name: "first-agent".to_string(),
                path: PathBuf::from("agents/first.agent.yaml"),
                build_hash_short: Some("abc123".to_string()),
            },
            ops::OpsBuild {
                name: "second-agent".to_string(),
                path: PathBuf::from("agents/second.agent.yaml"),
                build_hash_short: None,
            },
        ];
        app.focus = Focus::OpsBuildsList;
        app.selected_ops_build = 1;

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("first-agent"));
        assert!(rendered.contains("second-agent"));
        assert!(rendered.contains("abc123"));
        assert!(rendered.contains("Select a build and press Enter to run it."));
        assert!(!rendered.contains("Builds (2)"));
    }
}
