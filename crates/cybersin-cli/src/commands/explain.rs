//! `cybersin explain <prompt>`: compiled-prompt explanation and local
//! operations control room. The interactive and plain renderers share one
//! view model so redirected output remains useful and testable.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use ratatui::Terminal;

use crate::capabilities::{execute_explain_snapshot, ExplainInput, ExplainSnapshot};
use crate::session_liveness::{display_liveness, heartbeat_display, holder_display, now_unix_ms};

#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// Compiled prompt name, matching `dist/prompts/<name>.json`.
    pub prompt: String,
    /// Project directory containing `dist/`.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Print a stable text report instead of opening the interactive TUI.
    #[arg(long)]
    pub plain: bool,
}

type ExplainModel = ExplainSnapshot;

pub async fn execute(db: PathBuf, args: ExplainArgs) -> Result<()> {
    let daemon = cybersin_runtime::DaemonHandle::auto_start(db).await?;
    let model = ExplainModel::load(&args.path, &args.prompt, &daemon).await?;
    if args.plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!("{}", model.plain_report());
        return Ok(());
    }
    run_tui(&model)
}

impl ExplainModel {
    async fn load(
        project: &std::path::Path,
        prompt_name: &str,
        daemon: &cybersin_runtime::DaemonHandle,
    ) -> Result<Self> {
        execute_explain_snapshot(daemon, ExplainInput::new(project, prompt_name)).await
    }

    fn explain_text(&self) -> String {
        let mut out = format!(
            "Cybersin Explain: {}\n\nSection tokens by target\n",
            self.prompt
        );
        for target in &self.targets {
            let total: usize = target.sections.iter().map(|(_, tokens)| tokens).sum();
            out.push_str(&format!("  {} (total {total})\n", target.target));
            for (section, tokens) in &target.sections {
                out.push_str(&format!("    {section:<24} {tokens:>6}\n"));
            }
        }
        out.push_str("\nTool execution\n");
        if self.tools.is_empty() {
            out.push_str("  no compiled tool policy for this prompt\n");
        }
        for (name, policy) in &self.tools {
            let kind = if policy.is_builtin() {
                "built-in".to_string()
            } else {
                format!(
                    "custom image={} run={}",
                    policy.image,
                    policy.run.as_ref().unwrap().join(" ")
                )
            };
            let egress = if policy.egress.is_empty() {
                "none".to_string()
            } else {
                policy.egress.join(",")
            };
            out.push_str(&format!(
                "  {name}: {kind} scope={} egress=[{egress}] \
limits=cpu:{},mem_mb:{},wall_s:{}\n",
                policy.sandbox_scope, policy.cpu, policy.mem_mb, policy.wall_s
            ));
        }
        out.push_str("\nRouting\n");
        for line in &self.routing {
            out.push_str(&format!("  {line}\n"));
        }
        out.push_str(&format!(
            "  Estimated: ${:.6} per routed call\n",
            self.estimated_cost
        ));
        match &self.effective {
            Some((model, cost)) => out.push_str(&format!(
                "  Effective (this environment): {model} — ${cost:.6}\n"
            )),
            None => out.push_str(
                "  Effective (this environment): none — every candidate is disallowed by \
                 cybersin.local.yaml\n",
            ),
        }
        if self.observed_calls == 0 {
            out.push_str("  Observed: no matching LLM calls yet\n");
        } else {
            let noun = if self.observed_calls == 1 {
                "call"
            } else {
                "calls"
            };
            out.push_str(&format!(
                "  Observed: ${:.6} across {} LLM {noun}\n",
                self.observed_cost, self.observed_calls
            ));
        }
        out
    }

    fn sessions_text(&self) -> String {
        let mut out = format!("Sessions ({})\n", self.sessions.len());
        if self.sessions.is_empty() {
            out.push_str("  no sessions recorded\n");
        }
        let now = now_unix_ms();
        for session in &self.sessions {
            out.push_str(&format!(
                "  {}  {}  {}  {}  {}  {}  {}\n",
                session.session_id,
                session.status,
                session.agent_name,
                session.config_hash,
                display_liveness(session, now),
                heartbeat_display(session),
                holder_display(session)
            ));
        }
        out
    }

    fn traces_text(&self) -> String {
        let mut out = format!("Recent traces ({})\n", self.spans.len());
        if self.spans.is_empty() {
            out.push_str("  no spans recorded\n");
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
            "{}\nControl room\n\n{}\n{}\n{}",
            self.explain_text(),
            self.sessions_text(),
            self.traces_text(),
            self.costs_text()
        )
    }
}

fn run_tui(model: &ExplainModel) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let result = Terminal::new(backend)
        .map_err(anyhow::Error::from)
        .and_then(|mut terminal| tui_loop(&mut terminal, model));
    let cleanup = (|| -> Result<()> {
        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen, Show)?;
        Ok(())
    })();
    result.and(cleanup)
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: &ExplainModel,
) -> Result<()> {
    let titles = ["Explain", "Sessions", "Traces", "Cost"];
    let pages = explain_tui_pages(model);
    let mut selected = 0;
    loop {
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
                        .title(" Cybersin control room · ←/→ switch · q quit "),
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
                    KeyCode::Right => selected = (selected + 1).min(pages.len() - 1),
                    KeyCode::Char('1'..='4') => {
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

fn explain_tui_pages(model: &ExplainModel) -> [String; 4] {
    [
        model.explain_text(),
        model.sessions_text(),
        model.traces_text(),
        model.costs_text(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::execute_explain_snapshot;

    fn fixture_project() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/ic1-research-team")
            .canonicalize()
            .expect("fixture project should exist")
    }

    #[tokio::test]
    async fn plain_and_tui_pages_consume_the_explain_capability_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("explain-capability.db");
        let daemon = cybersin_runtime::DaemonHandle::auto_start(&db)
            .await
            .unwrap();
        let project = fixture_project();

        let direct = execute_explain_snapshot(&daemon, ExplainInput::new(&project, "researcher"))
            .await
            .unwrap();
        let command_model = ExplainModel::load(&project, "researcher", &daemon)
            .await
            .unwrap();

        assert_eq!(command_model.plain_report(), direct.plain_report());
        assert_eq!(
            explain_tui_pages(&command_model),
            explain_tui_pages(&direct)
        );
    }
}
