//! Bare `cybersin`: a reusable Ratatui application shell. `Home` lists
//! the available workflows behind a real `ListState` cursor (arrow keys
//! move it, `Enter` opens the highlighted one). Every workflow opens the
//! same `Workspace` screen, which multiplexes `Convert`/`Build`/`Ops`
//! behind a `Tabs` bar (arrow keys switch tabs) rather than adding a new
//! top-level `Screen` per workflow — a later workflow joins as another
//! tab instead of another screen plus another `go_back` case.
//!
//! `Build` and `Ops` are read-only info panels — `Build` also runs a
//! real, Dev-profile build on demand. Neither touches the daemon that
//! `cybersin ops`'s live Sessions/Traces/Cost/Approvals view owns
//! (`crates/cybersin-cli/src/commands/ops.rs`): the `Ops` tab here only
//! `stat()`s `.cybersin/cybersin.db` for a last-activity timestamp, so
//! landing on this screen never starts a daemon for a project that
//! hasn't been run yet.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::Value;

use crate::commands::build::{self, BuildProfile, BuildProgress};
use crate::commands::convert::{
    self, ConvertReport, OpenRouterPromptConversionModel, PromptConversionModel,
};
use crate::project::{self, ProjectDefaults};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Workspace,
}

/// The tab selected inside `Screen::Workspace`. `Convert` is the only
/// one with editable fields, so `Focus` only ever varies while this is
/// `Convert` — `Build`/`Ops` always sit at `Focus::Navigation`.
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

struct HomeItem {
    title: &'static str,
    description: &'static str,
    tab: WorkspaceTab,
}

/// `Home`'s list, in cursor order. Each entry opens `Workspace` on its
/// paired tab, so adding a fourth workflow later means adding both a
/// `WorkspaceTab` variant and a row here — never a new `Screen`.
const HOME_ITEMS: [HomeItem; 3] = [
    HomeItem {
        title: "Convert prompt",
        description: "raw prompt to buildable *.prompt.yaml",
        tab: WorkspaceTab::Convert,
    },
    HomeItem {
        title: "Build",
        description: "compile prompts + tools into dist/",
        tab: WorkspaceTab::Build,
    },
    HomeItem {
        title: "Ops",
        description: "project state at a glance — dist/ and recorded sessions",
        tab: WorkspaceTab::Ops,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Navigation,
    Prompt,
    Model,
    Out,
    ConvertAction,
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

#[derive(Debug)]
struct App {
    project_start: PathBuf,
    screen: Screen,
    workspace_tab: WorkspaceTab,
    focus: Focus,
    home_cursor: usize,
    raw_prompt: String,
    model: String,
    out: String,
    convert_status: ConversionStatus,
    build_status: BuildStatus,
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
        Self {
            project_start,
            screen: Screen::Home,
            workspace_tab: WorkspaceTab::Convert,
            focus: Focus::Navigation,
            home_cursor: 0,
            raw_prompt: String::new(),
            model: convert::DEFAULT_MODEL.to_string(),
            out: String::new(),
            convert_status: ConversionStatus::Idle,
            build_status: BuildStatus::Idle,
            show_help: false,
            should_quit: false,
        }
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

enum AppAction {
    None,
    Convert,
    Build,
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
            KeyCode::Esc => {
                self.go_back();
                AppAction::None
            }
            KeyCode::Left if self.screen == Screen::Workspace => {
                let tab = self.workspace_tab.previous();
                self.switch_tab(tab);
                AppAction::None
            }
            KeyCode::Right if self.screen == Screen::Workspace => {
                let tab = self.workspace_tab.next();
                self.switch_tab(tab);
                AppAction::None
            }
            KeyCode::Up if self.screen == Screen::Home => {
                self.move_home_cursor(-1);
                AppAction::None
            }
            KeyCode::Down if self.screen == Screen::Home => {
                self.move_home_cursor(1);
                AppAction::None
            }
            KeyCode::Tab => {
                self.focus_next();
                AppAction::None
            }
            KeyCode::BackTab => {
                self.focus_previous();
                AppAction::None
            }
            KeyCode::Char('q') if self.focus != Focus::Prompt => {
                self.should_quit = true;
                AppAction::None
            }
            KeyCode::Enter if self.screen == Screen::Home => {
                self.open_selected();
                AppAction::None
            }
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

    fn move_home_cursor(&mut self, delta: isize) {
        let len = HOME_ITEMS.len() as isize;
        let next = (self.home_cursor as isize + delta).rem_euclid(len);
        self.home_cursor = next as usize;
    }

    fn open_selected(&mut self) {
        let tab = HOME_ITEMS[self.home_cursor].tab;
        self.screen = Screen::Workspace;
        self.switch_tab(tab);
    }

    fn switch_tab(&mut self, tab: WorkspaceTab) {
        self.workspace_tab = tab;
        self.focus = match tab {
            WorkspaceTab::Convert => Focus::Prompt,
            WorkspaceTab::Build | WorkspaceTab::Ops => Focus::Navigation,
        };
    }

    fn request_action(&mut self) -> AppAction {
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
            WorkspaceTab::Ops => AppAction::None,
        }
    }

    fn go_back(&mut self) {
        match self.screen {
            Screen::Home => self.should_quit = true,
            Screen::Workspace => {
                self.screen = Screen::Home;
                self.focus = Focus::Navigation;
            }
        }
    }

    fn focus_next(&mut self) {
        if self.screen != Screen::Workspace || self.workspace_tab != WorkspaceTab::Convert {
            return;
        }
        self.focus = match self.focus {
            Focus::Prompt => Focus::Model,
            Focus::Model => Focus::Out,
            Focus::Out => Focus::ConvertAction,
            _ => Focus::Prompt,
        };
    }

    fn focus_previous(&mut self) {
        if self.screen != Screen::Workspace || self.workspace_tab != WorkspaceTab::Convert {
            return;
        }
        self.focus = match self.focus {
            Focus::Prompt => Focus::ConvertAction,
            Focus::Model => Focus::Prompt,
            Focus::Out => Focus::Model,
            _ => Focus::Out,
        };
    }

    fn insert_char(&mut self, ch: char) {
        match self.focus {
            Focus::Prompt => self.raw_prompt.push(ch),
            Focus::Model => self.model.push(ch),
            Focus::Out => self.out.push(ch),
            _ => return,
        }
        self.convert_status = ConversionStatus::Idle;
    }

    fn backspace(&mut self) {
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
                AppAction::None => {}
            }
        }
    }
    Ok(())
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
    let converter = OpenRouterPromptConversionModel::from_env(app.model.trim().to_string())?;
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
    app.build_status = BuildStatus::Running(vec!["starting dev build".to_string()]);
    terminal.draw(|frame| render(frame, app))?;

    let project_start = app.project_start.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = run_build_from(project_start, |progress| {
            let _ = tx.send(BuildThreadMessage::Progress(progress));
        });
        let _ = tx.send(BuildThreadMessage::Done(result));
    });

    loop {
        while let Ok(message) = rx.try_recv() {
            match message {
                BuildThreadMessage::Progress(progress) => push_build_progress(app, progress),
                BuildThreadMessage::Done(result) => {
                    app.build_status = match result {
                        Ok(message) => BuildStatus::Success(message),
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

fn push_build_progress(app: &mut App, progress: BuildProgress) {
    let line = format_build_progress(&app.project_start, progress);
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
    }

    frame.render_widget(Paragraph::new(footer_text(app)), chunks[2]);
    if app.show_help {
        render_help(frame, area);
    }
}

fn render_home(frame: &mut Frame, app: &App, area: Rect) {
    render_control_room_backdrop(frame, area);

    let panel = centered_rect(area, 64, HOME_ITEMS.len() as u16 + 2);
    frame.render_widget(Clear, pad_rect(panel, 1, area));

    let items: Vec<ListItem> = HOME_ITEMS
        .iter()
        .map(|item| {
            ListItem::new(Line::from(vec![
                Span::styled(item.title, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!("  {}", item.description)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.home_cursor));
    let list = List::new(items)
        .style(Style::default().bg(Color::Black))
        .block(focused_block(" Workflows ", true))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(list, panel, &mut state);
}

/// A procedurally generated wall of glowing "monitors" behind the
/// `Workflows` panel — an original, terminal-native stand-in for
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
                    Span::styled(ch.to_string(), Style::default().fg(color))
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
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
/// layout, also used to keep the `Workflows` panel a fixed, readable
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
/// — a blank "halo" cleared around the `Workflows` panel so it reads as
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

    frame.render_widget(
        Paragraph::new(build_info_text(&app.project_start))
            .block(Block::default().borders(Borders::ALL).title(" dist/ "))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    let action = if matches!(app.build_status, BuildStatus::Running(_)) {
        "Building..."
    } else {
        "Build (profile: dev)  Ctrl+R / F5"
    };
    frame.render_widget(
        Paragraph::new(action).block(Block::default().borders(Borders::ALL).title(" Action ")),
        chunks[1],
    );
    frame.render_widget(build_status_widget(&app.build_status), chunks[2]);
}

fn render_ops(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Min(3)])
        .split(area);

    frame.render_widget(
        Paragraph::new(ops_info_text(&app.project_start))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Project state "),
            )
            .wrap(Wrap { trim: false }),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(
            "Static snapshot only. Run `cybersin ops` for live sessions, traces, cost, and approvals.",
        )
        .block(Block::default().borders(Borders::ALL).title(" Ops "))
        .wrap(Wrap { trim: false }),
        chunks[1],
    );
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

/// `Build` tab's info panel: which project this shell is standing
/// inside of, plus what `dist/manifest.json` (if any) says about it.
fn build_info_text(project_start: &Path) -> String {
    match resolve_project_root(project_start) {
        Ok(root) => match dist_snapshot(&root) {
            Ok(snapshot) => format!(
                "project: {}\n\n{}\n\n{}",
                root.display(),
                prompt_sources_snapshot(&root),
                snapshot
            ),
            Err(error) => format!(
                "project: {}\n\n{}\n\n{}",
                root.display(),
                prompt_sources_snapshot(&root),
                error
            ),
        },
        Err(error) => error,
    }
}

fn prompt_sources_snapshot(project_root: &Path) -> String {
    match cybersin_frontend::discover_prompt_sources(project_root) {
        Ok(sources) if sources.is_empty() => "prompts: none found".to_string(),
        Ok(sources) => {
            let mut lines = vec![format!("prompts: {}", sources.len())];
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
fn ops_info_text(project_start: &Path) -> String {
    match resolve_project_root(project_start) {
        Ok(root) => match ops_snapshot(&root) {
            Ok(snapshot) => format!("project: {}\n\n{}", root.display(), snapshot),
            Err(error) => format!("project: {}\n\n{}", root.display(), error),
        },
        Err(error) => error,
    }
}

fn dist_snapshot(project_root: &Path) -> Result<String, String> {
    let defaults = ProjectDefaults::detect(project_root).map_err(|e| e.to_string())?;
    let dist = defaults.dist_default().map_err(|e| e.to_string())?;
    let manifest_path = dist.join("manifest.json");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("error: reading {}: {e}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_str(&text)
        .map_err(|e| format!("error: invalid {}: {e}", manifest_path.display()))?;
    let git_sha = manifest
        .get("git_sha")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let build_hash = manifest
        .get("build_hash")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let artifact_count = manifest
        .get("artifacts")
        .and_then(Value::as_object)
        .map_or(0, |artifacts| artifacts.len());
    Ok(format!(
        "dist/ at {}\n  git sha: {}\n  build hash: {}\n  artifacts: {}",
        dist.display(),
        short_hash(git_sha),
        short_hash(build_hash),
        artifact_count
    ))
}

fn ops_snapshot(project_root: &Path) -> Result<String, String> {
    let defaults = ProjectDefaults::detect(project_root).map_err(|e| e.to_string())?;
    let db_path = defaults.db_default();
    let text = match std::fs::metadata(&db_path) {
        Ok(meta) => {
            let age = meta
                .modified()
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
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
        Screen::Home => "Enter open \u{00b7} \u{2191}/\u{2193} select \u{00b7} ? help \u{00b7} q quit".to_string(),
        Screen::Workspace => match app.workspace_tab {
            WorkspaceTab::Convert => {
                "Ctrl+R/F5 convert \u{00b7} Tab/Shift-Tab focus \u{00b7} \u{2190}/\u{2192} tab \u{00b7} Enter type/act \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string()
            }
            WorkspaceTab::Build => {
                "Ctrl+R/F5 build \u{00b7} \u{2190}/\u{2192} tab \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string()
            }
            WorkspaceTab::Ops => "\u{2190}/\u{2192} tab \u{00b7} Esc back \u{00b7} ? help \u{00b7} q quit".to_string(),
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
            "Ctrl+R or F5 runs the active tab's primary action (convert or build)\n\u{2191}/\u{2193} moves the Home cursor \u{00b7} \u{2190}/\u{2192} switches Convert/Build/Ops tabs\nTab / Shift-Tab moves focus inside Convert\nEnter opens the highlighted Home item or runs the focused Convert action\nEsc goes back or dismisses overlays\nq quits when focus is outside the prompt editor\n-help, -h, and --help print CLI help",
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
    fn home_cursor_wraps_with_up_and_down() {
        let mut app = App::default();
        assert_eq!(app.home_cursor, 0);

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.home_cursor, HOME_ITEMS.len() - 1);

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.home_cursor, 0);
    }

    #[test]
    fn enter_opens_the_highlighted_home_item() {
        let mut app = App::default();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Workspace);
        assert_eq!(app.workspace_tab, WorkspaceTab::Build);
    }

    #[test]
    fn left_right_cycle_workspace_tabs_and_wrap() {
        let mut app = App::default();
        app.open_selected();
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
        app.open_selected();

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
        app.open_selected();

        app.handle_key(key(KeyCode::Char('q')));

        assert!(!app.should_quit);
        assert_eq!(app.raw_prompt, "q");
    }

    #[test]
    fn ctrl_r_converts_from_the_prompt_editor() {
        let mut app = App::default();
        app.open_selected();
        app.raw_prompt = "Turn this into a prompt source.".to_string();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::Convert));
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn empty_prompt_conversion_is_rejected_before_model_call() {
        let mut app = App::default();
        app.open_selected();

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
        app.home_cursor = 1;
        app.open_selected();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::Build));
    }

    #[test]
    fn ops_tab_has_no_primary_action() {
        let mut app = App::default();
        app.home_cursor = 2;
        app.open_selected();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::None));
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
        assert!(info.contains("prompts: 1"));
        assert!(info.contains("prompts/hello.prompt.yaml"));
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
        assert!(anchored_text.contains("no dist/ found"));

        let ops_text = ops_info_text(&nested);
        assert!(ops_text.contains(&format!("project: {}", project.path().display())));
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
        app.open_selected();
        app.raw_prompt = "Line one\nLine two".to_string();
        app.convert_status = ConversionStatus::Failure("network failed".to_string());
        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Raw Prompt"));
        assert!(rendered.contains("network failed"));
    }

    #[test]
    fn home_backdrop_surrounds_the_workflows_panel_without_hiding_it() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::default();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Workflows"));
        assert!(rendered.contains("Convert prompt"));
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
        app.open_selected();

        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Convert"));
        assert!(rendered.contains("Build"));
        assert!(rendered.contains("Ops"));
    }
}
