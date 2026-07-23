//! `cybersin init <dir>` (spec §11, §5): scaffolds a project layout —
//! `cybersin.yaml`, `cybersin.lock`, `prompts/`, `fragments/`, `evals/`,
//! `agents/`, `dist/` — minimal but real: the scaffolded
//! `prompts/hello.prompt.yaml` is a working source with an `!include`d
//! fragment, so `cybersin check <dir>` passes against it immediately.

use std::fs;
use std::path::Path;

const CYBERSIN_YAML: &str = r#"# Cybersin project config (spec §5, §6.3 cost model).
name: myagent
targets:
  - generic
cost_model:
  # Cold-start cache-similarity threshold and judge-trigger band (spec
  # §6.3): no observed traces exist yet on a first build, so these start
  # at conservative static defaults, biased toward false cache-misses
  # (never false cache-hits). `cybersin optimize` tightens or loosens
  # them later from real trace data.
  cache_similarity_threshold: 0.97
  judge_trigger_band: [0.90, 0.97]
storage:
  backend: sqlite
sandbox:
  backend: docker+gvisor
"#;

const CYBERSIN_LOCK: &str = r#"# Pinned models, prices, embedding model, and model-assisted pass
# outputs (spec §7). Empty until a build runs a model-assisted pass or
# `cybersin lock update` pins something.
models: {}
prices: {}
passes: {}
"#;

const HELLO_PROMPT: &str = r#"name: hello
quality: medium
inputs:
  name: string
sections:
  - id: role
    priority: 100
    body: !include ../fragments/tone.md
  - id: instructions
    priority: 90
    body: |
      Greet {{ name }} warmly and briefly.
"#;

const TONE_FRAGMENT: &str = "You are a friendly, concise assistant.\n";

const HELLO_EVAL: &str = r#"# Eval source (spec §5.2). Eval compilation is a later issue; this file
# is scaffolding only.
prompt: hello
cases:
  - name: basic_greeting
    inputs: { name: "Ada" }
    assertions:
      - type: contains_none
        values: ["error", "sorry"]
runs_per_case: 1
"#;

const HELLO_AGENT: &str = r#"# Agent runtime config (spec §5.3). Runtime consumption is a later
# issue; this file is scaffolding only.
name: hello-agent
harness: { adapter: process, command: ["python", "loop.py"] }
budget: { usd_per_session: 1.00, on_breach: degrade }
tools: []
"#;

pub fn run(dir: &Path) -> Result<Option<String>, String> {
    let write = |rel: &str, contents: &str| -> Result<(), String> {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("error: failed to create {}: {e}", parent.display()))?;
        }
        fs::write(&path, contents)
            .map_err(|e| format!("error: failed to write {}: {e}", path.display()))
    };

    write("cybersin.yaml", CYBERSIN_YAML)?;
    write("cybersin.lock", CYBERSIN_LOCK)?;
    write("prompts/hello.prompt.yaml", HELLO_PROMPT)?;
    write("fragments/tone.md", TONE_FRAGMENT)?;
    write("evals/hello.eval.yaml", HELLO_EVAL)?;
    write("agents/hello.agent.yaml", HELLO_AGENT)?;

    let dist = dir.join("dist");
    fs::create_dir_all(&dist)
        .map_err(|e| format!("error: failed to create {}: {e}", dist.display()))?;

    Ok(Some(format!(
        "scaffolded a new cybersin project at {}",
        dir.display()
    )))
}
