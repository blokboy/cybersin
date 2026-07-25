//! `cybersin ops [path]`: live-refreshing Sessions/Traces/Cost control
//! room (issue #51). Follows `explain`'s "one view model, two renderers"
//! shape (`crates/cybersin-cli/src/commands/explain.rs`) for the
//! interactive TUI + `--plain`/non-TTY text fallback, and reuses its
//! Sessions/Traces/Cost query patterns verbatim. Two things differ from
//! `explain`: there's no compiled prompt to explain (`ops` is
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

use std::io::{self, IsTerminal};
use std::path::PathBuf;
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
use cybersin_runtime::{DaemonHandle, SessionRecord};
use cybersin_trace::{CostDimension, CostRollupRow, Span, SpanFilter};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use ratatui::Terminal;

use crate::project::ProjectDefaults;

/// How often the TUI's Sessions/Traces/Cost tabs re-query the daemon.
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

#[derive(Debug, Clone, Default)]
struct OpsModel {
    sessions: Vec<SessionRecord>,
    spans: Vec<Span>,
    costs: Vec<CostRollupRow>,
}

impl OpsModel {
    async fn load(daemon: &DaemonHandle) -> Result<Self> {
        let spans = daemon
            .spans()
            .list(&SpanFilter {
                limit: Some(1_000),
                ..SpanFilter::default()
            })
            .await?;
        Ok(Self {
            sessions: daemon.storage().list_sessions().await?,
            costs: daemon.spans().cost_rollup(CostDimension::Model).await?,
            spans: spans.into_iter().take(25).collect(),
        })
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

    fn plain_report(&self) -> String {
        format!(
            "{}\n{}\n{}",
            self.sessions_text(),
            self.traces_text(),
            self.costs_text()
        )
    }
}

pub async fn execute(db: Option<PathBuf>, args: OpsArgs) -> Result<()> {
    let start = std::fs::canonicalize(&args.path)
        .with_context(|| format!("resolving project path {}", args.path.display()))?;
    let defaults = ProjectDefaults::detect(&start)?;
    let db = db.unwrap_or_else(|| defaults.db_default());

    let daemon = DaemonHandle::auto_start(db).await?;
    let model = OpsModel::load(&daemon).await?;

    if args.plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!("{}", model.plain_report());
        return Ok(());
    }

    run_tui(daemon, model).await
}

/// Runs the interactive TUI. A background task refreshes `shared` from
/// the daemon every [`REFRESH_INTERVAL`]; the terminal loop itself stays
/// synchronous (crossterm's input model isn't async here) and simply
/// redraws from whatever `shared` currently holds on every iteration —
/// which happens at least every 250ms regardless of key input — so a
/// refresh lands on screen without the user ever leaving `ops`.
async fn run_tui(daemon: DaemonHandle, initial: OpsModel) -> Result<()> {
    let shared = Arc::new(Mutex::new(initial));

    let refresh_shared = Arc::clone(&shared);
    let refresh_daemon = daemon.clone();
    let refresh_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
        ticker.tick().await; // first tick fires immediately; the initial load already covers it
        loop {
            ticker.tick().await;
            if let Ok(model) = OpsModel::load(&refresh_daemon).await {
                *refresh_shared.lock().unwrap() = model;
            }
        }
    });

    let ui_shared = Arc::clone(&shared);
    let ui_result = tokio::task::spawn_blocking(move || run_terminal_loop(&ui_shared)).await;
    refresh_task.abort();

    ui_result.context("ops terminal task panicked")?
}

fn run_terminal_loop(shared: &Arc<Mutex<OpsModel>>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let result = Terminal::new(backend)
        .map_err(anyhow::Error::from)
        .and_then(|mut terminal| tui_loop(&mut terminal, shared));
    let cleanup = (|| -> Result<()> {
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, Show)?;
        Ok(())
    })();
    result.and(cleanup)
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    shared: &Arc<Mutex<OpsModel>>,
) -> Result<()> {
    let titles = ["Sessions", "Traces", "Cost"];
    let mut selected = 0;
    loop {
        let pages = {
            let model = shared.lock().unwrap();
            [
                model.sessions_text(),
                model.traces_text(),
                model.costs_text(),
            ]
        };
        terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(1)])
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
            frame.render_widget(
                Paragraph::new(pages[selected].as_str())
                    .block(Block::default().borders(Borders::ALL))
                    .wrap(Wrap { trim: false }),
                areas[1],
            );
        })?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Left => selected = selected.saturating_sub(1),
                    KeyCode::Right => selected = (selected + 1).min(titles.len() - 1),
                    KeyCode::Char('1'..='3') => {
                        if let KeyCode::Char(number) = key.code {
                            selected = number as usize - '1' as usize;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
