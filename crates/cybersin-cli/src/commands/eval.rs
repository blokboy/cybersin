//! Eval-source compilation and recorded/live runner (spec §5.2, §8.6).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use cybersin_ir::{InputType, PromptIr};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    /// Compile and execute eval suites, printing N-run score distributions.
    Run(EvalArgs),
    /// Execute eval suites and fail if any run violates an assertion.
    Gate(EvalArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Provider {
    /// Deterministic checked-in samples; the CI default.
    Recorded,
    /// Invoke the command in CYBERSIN_EVAL_PROVIDER for each sample.
    Live,
}

#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Project directory containing evals/ and dist/prompts/.
    #[arg(default_value = ".")]
    pub path: PathBuf,
    #[arg(long, value_enum, default_value = "recorded")]
    pub provider: Provider,
}

#[derive(Debug, Deserialize)]
struct EvalSource {
    prompt: String,
    cases: Vec<EvalCase>,
    #[serde(default = "one")]
    runs_per_case: usize,
}

fn one() -> usize {
    1
}

#[derive(Debug, Deserialize)]
struct EvalCase {
    name: String,
    inputs: BTreeMap<String, Value>,
    assertions: Vec<Assertion>,
    #[serde(default)]
    recorded_outputs: Vec<RecordedSample>,
}

#[derive(Debug, Clone, Deserialize)]
struct RecordedSample {
    output: Value,
    #[serde(default)]
    judge_score: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Assertion {
    JsonValid,
    ContainsNone { values: Vec<String> },
    Judge { rubric: String, min_score: f64 },
    Custom { command: String },
}

struct CompiledSuite {
    source_path: PathBuf,
    source: EvalSource,
}

struct CaseResult {
    suite: String,
    case: String,
    scores: Vec<f64>,
    passed: bool,
}

pub async fn execute(command: EvalCommand) -> Result<()> {
    let (args, gate) = match command {
        EvalCommand::Run(args) => (args, false),
        EvalCommand::Gate(args) => (args, true),
    };
    let suites = compile_project(&args.path)?;
    let mut results = Vec::new();
    for suite in suites {
        results.extend(run_suite(&suite, args.provider)?);
    }
    if results.is_empty() {
        anyhow::bail!(
            "no eval cases found under {}",
            args.path.join("evals").display()
        );
    }
    let mut failed = false;
    for result in &results {
        let min = result.scores.iter().copied().fold(f64::INFINITY, f64::min);
        let max = result
            .scores
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let mean = result.scores.iter().sum::<f64>() / result.scores.len() as f64;
        println!(
            "{}::{} runs={} min={min:.3} mean={mean:.3} max={max:.3} {}",
            result.suite,
            result.case,
            result.scores.len(),
            if result.passed { "PASS" } else { "FAIL" }
        );
        failed |= !result.passed;
    }
    if gate && failed {
        anyhow::bail!("eval gate failed: one or more runs regressed");
    }
    Ok(())
}

fn compile_project(project: &Path) -> Result<Vec<CompiledSuite>> {
    let eval_dir = project.join("evals");
    let mut paths = fs::read_dir(&eval_dir)
        .with_context(|| format!("reading {}", eval_dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".eval.yaml"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut suites = Vec::new();
    for path in paths {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let source: EvalSource = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing eval source {}", path.display()))?;
        if source.runs_per_case == 0 {
            anyhow::bail!(
                "{}: runs_per_case must be greater than zero",
                path.display()
            );
        }
        let prompt_path = project
            .join("dist/prompts")
            .join(format!("{}.json", source.prompt));
        let prompt: PromptIr = serde_json::from_slice(
            &fs::read(&prompt_path)
                .with_context(|| format!("reading compiled prompt {}", prompt_path.display()))?,
        )
        .with_context(|| format!("parsing compiled prompt {}", prompt_path.display()))?;
        for case in &source.cases {
            validate_case(&path, case, &prompt)?;
        }
        suites.push(CompiledSuite {
            source_path: path,
            source,
        });
    }
    Ok(suites)
}

fn validate_case(path: &Path, case: &EvalCase, prompt: &PromptIr) -> Result<()> {
    for (name, ty) in &prompt.inputs {
        let value = case.inputs.get(name).with_context(|| {
            format!(
                "{} case {:?}: missing required input {name:?}",
                path.display(),
                case.name
            )
        })?;
        if !value_matches(value, ty) {
            anyhow::bail!(
                "{} case {:?}: input {name:?} does not match {}",
                path.display(),
                case.name,
                input_type_name(ty)
            );
        }
    }
    for name in case.inputs.keys() {
        if !prompt.inputs.contains_key(name) {
            anyhow::bail!(
                "{} case {:?}: unknown input {name:?}",
                path.display(),
                case.name
            );
        }
    }
    for assertion in &case.assertions {
        match assertion {
            Assertion::Judge { rubric, min_score } => {
                if rubric.trim().is_empty() || !(0.0..=1.0).contains(min_score) {
                    anyhow::bail!(
                        "{} case {:?}: judge requires a rubric and min_score in 0..=1",
                        path.display(),
                        case.name
                    );
                }
            }
            Assertion::ContainsNone { values } if values.is_empty() => anyhow::bail!(
                "{} case {:?}: contains_none requires at least one value",
                path.display(),
                case.name
            ),
            Assertion::Custom { command } if command.trim().is_empty() => anyhow::bail!(
                "{} case {:?}: custom assertion command must not be empty",
                path.display(),
                case.name
            ),
            _ => {}
        }
    }
    Ok(())
}

fn value_matches(value: &Value, ty: &InputType) -> bool {
    match ty {
        InputType::String => value.is_string(),
        InputType::Number => value.is_number(),
        InputType::Bool => value.is_boolean(),
        InputType::Document => value.is_object(),
        InputType::Enum { variants } => value
            .as_str()
            .is_some_and(|value| variants.iter().any(|variant| variant == value)),
        InputType::List { of } => value
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value_matches(value, of))),
    }
}

fn input_type_name(ty: &InputType) -> &'static str {
    match ty {
        InputType::String => "string",
        InputType::Number => "number",
        InputType::Bool => "bool",
        InputType::Document => "document",
        InputType::Enum { .. } => "enum",
        InputType::List { .. } => "list",
    }
}

fn run_suite(suite: &CompiledSuite, provider: Provider) -> Result<Vec<CaseResult>> {
    let mut results = Vec::new();
    for case in &suite.source.cases {
        let mut scores = Vec::with_capacity(suite.source.runs_per_case);
        for run in 0..suite.source.runs_per_case {
            let sample = sample_for(case, run, provider)?;
            let passed = case
                .assertions
                .iter()
                .map(|assertion| evaluate(assertion, &sample))
                .collect::<Result<Vec<_>>>()?;
            scores
                .push(passed.iter().filter(|passed| **passed).count() as f64 / passed.len() as f64);
        }
        results.push(CaseResult {
            suite: suite
                .source_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("eval")
                .to_string(),
            case: case.name.clone(),
            passed: scores
                .iter()
                .all(|score| (*score - 1.0).abs() < f64::EPSILON),
            scores,
        });
    }
    Ok(results)
}

fn sample_for(case: &EvalCase, run: usize, provider: Provider) -> Result<RecordedSample> {
    match provider {
        Provider::Recorded => {
            if case.recorded_outputs.is_empty() {
                return Ok(RecordedSample {
                    output: Value::String(serde_json::to_string(&case.inputs)?),
                    judge_score: None,
                });
            }
            Ok(case.recorded_outputs[run % case.recorded_outputs.len()].clone())
        }
        Provider::Live => {
            let command = std::env::var("CYBERSIN_EVAL_PROVIDER")
                .context("--provider live requires CYBERSIN_EVAL_PROVIDER to name an executable")?;
            let output = Command::new(command)
                .arg(serde_json::to_string(&case.inputs)?)
                .output()
                .context("running live eval provider")?;
            if !output.status.success() {
                anyhow::bail!("live eval provider exited {}", output.status);
            }
            let value: Value = serde_json::from_slice(&output.stdout)
                .context("live provider returned invalid JSON")?;
            serde_json::from_value(value).context(
                "live provider must return {\"output\": ..., \"judge_score\": optional number}",
            )
        }
    }
}

fn evaluate(assertion: &Assertion, sample: &RecordedSample) -> Result<bool> {
    let text = match &sample.output {
        Value::String(text) => text.clone(),
        value => serde_json::to_string(value)?,
    };
    match assertion {
        Assertion::JsonValid => {
            Ok(!sample.output.is_string() || serde_json::from_str::<Value>(&text).is_ok())
        }
        Assertion::ContainsNone { values } => {
            let folded = text.to_lowercase();
            Ok(values
                .iter()
                .all(|value| !folded.contains(&value.to_lowercase())))
        }
        Assertion::Judge { min_score, .. } => Ok(sample.judge_score.unwrap_or(0.0) >= *min_score),
        Assertion::Custom { command } => Ok(Command::new("sh")
            .args(["-c", command])
            .env("CYBERSIN_EVAL_OUTPUT", text)
            .status()
            .context("running custom eval assertion")?
            .success()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertions_score_recorded_samples() {
        let sample = RecordedSample {
            output: Value::String(r#"{"answer":"good"}"#.into()),
            judge_score: Some(0.9),
        };
        assert!(evaluate(&Assertion::JsonValid, &sample).unwrap());
        assert!(evaluate(
            &Assertion::ContainsNone {
                values: vec!["error".into()]
            },
            &sample
        )
        .unwrap());
        assert!(evaluate(
            &Assertion::Judge {
                rubric: "correct".into(),
                min_score: 0.8
            },
            &sample
        )
        .unwrap());
    }
}
