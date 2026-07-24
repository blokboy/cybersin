//! `cybersin explain <prompt>`: compiled-prompt explanation and local
//! operations control room. The interactive and plain renderers share one
//! view model so redirected output remains useful and testable.

use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use cybersin_backends::{backend_for, RenderedPrompt};
use cybersin_ir::PromptIr;
use cybersin_router::{RouteDecision, RouteModel, RoutingArtifact};
use cybersin_runtime::{DaemonHandle, ModelAllowlist, SessionRecord};
use cybersin_trace::{CostDimension, CostRollupRow, Span, SpanFilter, SpanKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use ratatui::Terminal;

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

#[derive(Debug)]
struct TargetTokens {
    target: String,
    sections: Vec<(String, usize)>,
}

#[derive(Debug)]
struct ExplainModel {
    prompt: String,
    targets: Vec<TargetTokens>,
    routing: Vec<String>,
    estimated_cost: f64,
    /// The first candidate this environment's `cybersin.local.yaml`
    /// allowlist would actually let a real run reach, computed at
    /// `explain`-invocation time rather than baked into `dist/` — issue
    /// #35 Phase 1: `dist/routing.json` stays portable across
    /// environments, but cost visibility before a run should reflect what
    /// *this* environment can actually call. `None` means every candidate
    /// in this project's routing is disallowed here.
    effective: Option<(String, f64)>,
    observed_cost: f64,
    observed_calls: usize,
    sessions: Vec<SessionRecord>,
    spans: Vec<Span>,
    costs: Vec<CostRollupRow>,
}

pub async fn execute(db: PathBuf, args: ExplainArgs) -> Result<()> {
    let daemon = DaemonHandle::auto_start(db).await?;
    let model = ExplainModel::load(&args.path, &args.prompt, &daemon).await?;
    if args.plain || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        print!("{}", model.plain_report());
        return Ok(());
    }
    run_tui(&model)
}

impl ExplainModel {
    async fn load(project: &Path, prompt_name: &str, daemon: &DaemonHandle) -> Result<Self> {
        let prompt_path = project
            .join("dist")
            .join("prompts")
            .join(format!("{prompt_name}.json"));
        let prompt: PromptIr = read_json(&prompt_path).with_context(|| {
            format!(
                "compiled prompt {:?} not found; run `cybersin build {}` first",
                prompt_name,
                project.display()
            )
        })?;

        let target_dir = project.join("dist").join("prompts").join(prompt_name);
        let mut rendered_targets = Vec::new();
        let entries = fs::read_dir(&target_dir).with_context(|| {
            format!(
                "rendered targets for {:?} not found; run `cybersin build {}` first",
                prompt_name,
                project.display()
            )
        })?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let rendered: RenderedPrompt = read_json(&path)
                .with_context(|| format!("reading backend output {}", path.display()))?;
            rendered_targets.push(rendered.target);
        }
        rendered_targets.sort();
        rendered_targets.dedup();
        if rendered_targets.is_empty() {
            anyhow::bail!("compiled prompt {prompt_name:?} has no rendered backend targets");
        }

        let mut targets = Vec::new();
        for target in rendered_targets {
            let backend = backend_for(&target).map_err(anyhow::Error::msg)?;
            let mut sections = Vec::new();
            for section in &prompt.sections {
                let tokens = if section.dedup_ref.is_some() {
                    0
                } else {
                    let mut section_prompt = prompt.clone();
                    section_prompt.sections = vec![section.clone()];
                    backend
                        .render(&section_prompt)
                        .map_err(anyhow::Error::msg)?
                        .messages
                        .iter()
                        .map(|message| message.content.split_whitespace().count())
                        .sum()
                };
                sections.push((section.id.clone(), tokens));
            }
            targets.push(TargetTokens { target, sections });
        }

        let routing_artifact: RoutingArtifact =
            read_json(&project.join("dist").join("routing.json"))
                .context("reading real dist/routing.json")?;
        let route = routing_artifact
            .prompts
            .get(prompt_name)
            .with_context(|| format!("routing.json has no route for prompt {prompt_name:?}"))?;
        let (routing, estimated_cost) = render_route(&route.decisions);
        let allowlist = ModelAllowlist::load(project).with_context(|| {
            format!("reading {}", project.join("cybersin.local.yaml").display())
        })?;
        let effective = effective_first_candidate(&route.decisions, &allowlist)
            .map(|model| (describe_model(&model), model.estimated_cost_usd));

        let all_spans = daemon
            .spans()
            .list(&SpanFilter {
                limit: Some(1_000),
                ..SpanFilter::default()
            })
            .await?;
        let observed = all_spans
            .iter()
            .filter(|span| span.kind == SpanKind::LlmCall && span.name == prompt_name)
            .collect::<Vec<_>>();
        let observed_cost = observed.iter().map(|span| span.usd_cost).sum();
        let observed_calls = observed.len();

        Ok(Self {
            prompt: prompt.name,
            targets,
            routing,
            estimated_cost,
            effective,
            observed_cost,
            observed_calls,
            sessions: daemon.storage().list_sessions().await?,
            spans: all_spans.into_iter().take(25).collect(),
            costs: daemon.spans().cost_rollup(CostDimension::Model).await?,
        })
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

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn describe_model(model: &RouteModel) -> String {
    format!(
        "{} ({}, {:?}) — estimated ${:.6}",
        model.name, model.provider, model.quality, model.estimated_cost_usd
    )
}

fn render_route(decisions: &[RouteDecision]) -> (Vec<String>, f64) {
    let mut lines = Vec::new();
    let mut estimated = 0.0_f64;
    for decision in decisions {
        match decision {
            RouteDecision::Cache(cache) => {
                lines.push(format!(
                    "├─ cache ≥ {:.2}; judge {:.2}..{:.2}: {}",
                    cache.similarity_threshold,
                    cache.judge_trigger_band[0],
                    cache.judge_trigger_band[1],
                    describe_model(&cache.judge)
                ));
            }
            RouteDecision::Cascade(cascade) => {
                lines.push("├─ cascade".into());
                for (index, step) in cascade.steps.iter().enumerate() {
                    let branch = if index + 1 == cascade.steps.len() {
                        "└─"
                    } else {
                        "├─"
                    };
                    lines.push(format!(
                        "│  {branch} {} (accept ≥ {:.2})",
                        describe_model(&step.model),
                        step.confidence.minimum_score
                    ));
                    estimated += step.model.estimated_cost_usd;
                }
            }
            RouteDecision::Fallbacks(fallbacks) => {
                lines.push("└─ provider fallbacks".into());
                for provider in &fallbacks.providers {
                    lines.push(format!("   └─ {}", describe_model(provider)));
                }
            }
        }
    }
    (lines, estimated)
}

/// The first candidate a real run would actually reach in this
/// environment: walk cascade steps then provider fallbacks in order,
/// skipping anything `allowlist` disallows — the same skip
/// `RouteExecutor::execute` applies at call time (issue #35 Phase 1), just
/// simulated here instead of executed. Cache decisions are a zero-cost
/// early exit that don't call a model at all, so they're not part of
/// "effective candidate" — same reasoning `RouteExecutor` uses (they're
/// not gated by the allowlist either).
fn effective_first_candidate(
    decisions: &[RouteDecision],
    allowlist: &ModelAllowlist,
) -> Option<RouteModel> {
    for decision in decisions {
        match decision {
            RouteDecision::Cache(_) => {}
            RouteDecision::Cascade(cascade) => {
                if let Some(step) = cascade
                    .steps
                    .iter()
                    .find(|step| allowlist.allows(&step.model))
                {
                    return Some(step.model.clone());
                }
            }
            RouteDecision::Fallbacks(fallbacks) => {
                if let Some(model) = fallbacks
                    .providers
                    .iter()
                    .find(|model| allowlist.allows(model))
                {
                    return Some(model.clone());
                }
            }
        }
    }
    None
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
    let pages = [
        model.explain_text(),
        model.sessions_text(),
        model.traces_text(),
        model.costs_text(),
    ];
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
