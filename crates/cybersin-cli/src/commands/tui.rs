//! Bare `cybersin`: a reusable Ratatui application shell. Prompt
//! conversion is the first complete workflow, but the state model keeps
//! navigation separate from the conversion view so later workflows can
//! join the shell without replacing the event loop.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

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
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::commands::convert::{
    self, ConvertReport, OpenRouterPromptConversionModel, PromptConversionModel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Convert,
}

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

#[derive(Debug)]
struct App {
    screen: Screen,
    focus: Focus,
    raw_prompt: String,
    model: String,
    out: String,
    status: ConversionStatus,
    show_help: bool,
    should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            screen: Screen::Home,
            focus: Focus::Navigation,
            raw_prompt: String::new(),
            model: convert::DEFAULT_MODEL.to_string(),
            out: String::new(),
            status: ConversionStatus::Idle,
            show_help: false,
            should_quit: false,
        }
    }
}

enum AppAction {
    None,
    Convert,
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
                self.request_conversion()
            }
            KeyCode::F(5) => self.request_conversion(),
            KeyCode::Esc => {
                self.go_back();
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
                self.open_conversion();
                AppAction::None
            }
            KeyCode::Enter if self.focus == Focus::ConvertAction => self.request_conversion(),
            KeyCode::Enter if self.focus == Focus::Prompt => {
                self.raw_prompt.push('\n');
                self.status = ConversionStatus::Idle;
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

    fn open_conversion(&mut self) {
        self.screen = Screen::Convert;
        self.focus = Focus::Prompt;
    }

    fn request_conversion(&mut self) -> AppAction {
        if self.screen != Screen::Convert {
            return AppAction::None;
        }
        if self.raw_prompt.trim().is_empty() {
            self.status =
                ConversionStatus::Failure("Enter a prompt before converting.".to_string());
            AppAction::None
        } else {
            AppAction::Convert
        }
    }

    fn go_back(&mut self) {
        match self.screen {
            Screen::Home => self.should_quit = true,
            Screen::Convert => {
                self.screen = Screen::Home;
                self.focus = Focus::Navigation;
            }
        }
    }

    fn focus_next(&mut self) {
        self.focus = match (self.screen, self.focus) {
            (Screen::Home, _) => Focus::Navigation,
            (Screen::Convert, Focus::Prompt) => Focus::Model,
            (Screen::Convert, Focus::Model) => Focus::Out,
            (Screen::Convert, Focus::Out) => Focus::ConvertAction,
            (Screen::Convert, _) => Focus::Prompt,
        };
    }

    fn focus_previous(&mut self) {
        self.focus = match (self.screen, self.focus) {
            (Screen::Home, _) => Focus::Navigation,
            (Screen::Convert, Focus::Prompt) => Focus::ConvertAction,
            (Screen::Convert, Focus::Model) => Focus::Prompt,
            (Screen::Convert, Focus::Out) => Focus::Model,
            (Screen::Convert, _) => Focus::Out,
        };
    }

    fn insert_char(&mut self, ch: char) {
        match self.focus {
            Focus::Prompt => self.raw_prompt.push(ch),
            Focus::Model => self.model.push(ch),
            Focus::Out => self.out.push(ch),
            _ => return,
        }
        self.status = ConversionStatus::Idle;
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
        self.status = ConversionStatus::Idle;
    }

    fn conversion_out(&self) -> Option<PathBuf> {
        let trimmed = self.out.trim();
        (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
    }
}

pub async fn execute() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "bare `cybersin` requires an interactive terminal; use `cybersin -help` or an explicit subcommand for non-interactive use"
        );
    }
    let mut app = App::default();
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
            if let AppAction::Convert = app.handle_key(key) {
                app.status = ConversionStatus::Running;
                terminal.draw(|frame| render(frame, app))?;
                let result = run_conversion(app).await;
                app.status = match result {
                    Ok(report) => ConversionStatus::Success(report.into()),
                    Err(error) => ConversionStatus::Failure(error),
                };
            }
        }
    }
    Ok(())
}

async fn run_conversion(app: &App) -> Result<ConvertReport, String> {
    let cwd =
        std::env::current_dir().map_err(|e| format!("error: reading current directory: {e}"))?;
    let project_root = conversion_root(&cwd);
    let converter = OpenRouterPromptConversionModel::from_env(app.model.trim().to_string())?;
    run_conversion_with_model(&converter, &project_root, app).await
}

fn conversion_root(cwd: &Path) -> PathBuf {
    crate::project::discover_project_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
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
        Screen::Home => "Cybersin",
        Screen::Convert => "Cybersin / Convert",
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
        Screen::Convert => render_convert(frame, app, chunks[1]),
    }

    frame.render_widget(Paragraph::new(footer_text(app)), chunks[2]);
    if app.show_help {
        render_help(frame, area);
    }
}

fn render_home(frame: &mut Frame, app: &App, area: Rect) {
    let items = vec![
        ListItem::new(Line::from(vec![
            Span::styled(
                "Convert prompt",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  raw prompt to buildable *.prompt.yaml"),
        ])),
        ListItem::new("Build"),
        ListItem::new("Run"),
        ListItem::new("Ops"),
    ];
    let block = focused_block(" Workflows ", app.focus == Focus::Navigation);
    frame.render_widget(List::new(items).block(block), area);
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
    let action = if app.status == ConversionStatus::Running {
        "Converting..."
    } else {
        "Convert  Ctrl+R / F5"
    };
    frame.render_widget(
        Paragraph::new(action).block(focused_block(" Action ", app.focus == Focus::ConvertAction)),
        chunks[3],
    );
    frame.render_widget(status_widget(&app.status), chunks[4]);
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

fn status_widget(status: &ConversionStatus) -> Paragraph<'static> {
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

fn footer_text(app: &App) -> &'static str {
    match app.screen {
        Screen::Home => "Enter open · ? help · q quit",
        Screen::Convert => {
            "Ctrl+R/F5 convert · Tab/Shift-Tab focus · Enter type/act · Esc back · ? help · q quit"
        }
    }
}

fn render_help(frame: &mut Frame, area: Rect) {
    let width = area.width.min(74);
    let height = area.height.min(10);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(
            "Ctrl+R or F5 converts the raw prompt\nTab / Shift-Tab moves focus\nEnter opens or runs the focused action\nEsc goes back or dismisses overlays\nq quits when focus is outside the prompt editor\n-help, -h, and --help print CLI help",
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

        assert_eq!(app.screen, Screen::Convert);
        assert_eq!(app.raw_prompt, "Hi");
    }

    #[test]
    fn conversion_focus_cycles_through_fields() {
        let mut app = App::default();
        app.open_conversion();

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
        app.open_conversion();

        app.handle_key(key(KeyCode::Char('q')));

        assert!(!app.should_quit);
        assert_eq!(app.raw_prompt, "q");
    }

    #[test]
    fn ctrl_r_converts_from_the_prompt_editor() {
        let mut app = App::default();
        app.open_conversion();
        app.raw_prompt = "Turn this into a prompt source.".to_string();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::Convert));
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn empty_prompt_conversion_is_rejected_before_model_call() {
        let mut app = App::default();
        app.open_conversion();

        let action = app.handle_key(ctrl_key('r'));

        assert!(matches!(action, AppAction::None));
        assert_eq!(
            app.status,
            ConversionStatus::Failure("Enter a prompt before converting.".to_string())
        );
    }

    #[test]
    fn conversion_root_falls_back_to_cwd_without_project_file() {
        let cwd = tempfile::tempdir().unwrap();

        assert_eq!(conversion_root(cwd.path()), cwd.path());
    }

    #[test]
    fn conversion_root_prefers_discovered_project_root() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("notes/drafts");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.path().join("cybersin.yaml"), "name: test\n").unwrap();

        assert_eq!(conversion_root(&nested), project.path());
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

    #[test]
    fn render_home_and_small_conversion_layouts() {
        let backend = TestBackend::new(48, 14);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();

        terminal.draw(|frame| render(frame, &app)).unwrap();
        app.open_conversion();
        app.raw_prompt = "Line one\nLine two".to_string();
        app.status = ConversionStatus::Failure("network failed".to_string());
        terminal.draw(|frame| render(frame, &app)).unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("Raw Prompt"));
        assert!(rendered.contains("network failed"));
    }
}
