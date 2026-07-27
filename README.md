# Cybersin

Cybersin is a Rust-based prompt compiler and durable agent runtime that turns typed prompt sources into optimized, routable, cacheable artifacts. It gives agent developers one CLI for deterministic builds, regression evals, sandboxed execution, resumable sessions, cost tracing, and profile-guided optimization.

![Cybersin crate architecture](docs/assets/cybersin-crate-architecture.svg)

![Cybersin build and run data flow](docs/assets/cybersin-build-and-run-dataflow.svg)

## Getting Started

### Prerequisites

- Git
- A current stable Rust toolchain (`rustup`, `rustc`, and `cargo`)

Docker is only required for the container-backed sandbox tests; the quickstart below uses the local SQLite runtime.

### Install

Install the published CLI from crates.io:

```sh
cargo install cybersin
```

Then check the installed binary and command surface:

```sh
cybersin --help
```

To update an existing install later:

```sh
cargo install cybersin --force
```

For contributor builds from source:

```sh
git clone https://github.com/blokboy/cybersin.git
cd cybersin
cargo build --workspace
```

### Create a Project

`cybersin init <dir>` creates the basic Cybersin project spine. It creates
the directory if needed, writes the portable project files, and leaves prompt
sources and build output empty until you add them.

Preview the scaffold first:

```sh
cybersin init myagent --dry-run
```

The dry run reports exactly what would be written:

```sh
would scaffold cybersin project spine at myagent
created:
  cybersin.yaml
  cybersin.lock
  cybersin.local.example.yaml
  .gitignore
  prompts
  fragments
  evals
  agents
  tools
skipped: none
```

Create the files:

```sh
cybersin init myagent
cd myagent
```

The scaffold is intentionally minimal:

```text
myagent/
  cybersin.yaml                 # portable project config
  cybersin.lock                 # pinned model/cost/pass data
  cybersin.local.example.yaml   # example machine-local runtime config
  .gitignore                    # ignores local state, local config, dist
  prompts/                      # prompt sources live here
  fragments/                    # included markdown/json snippets
  evals/                        # regression eval fixtures
  agents/                       # live agent harness configs
  tools/                        # custom tool executables/assets
```

Plain `init` is safe for an existing codebase: existing files are skipped and
reported in the command output. Use `--force` only when you intentionally want
to overwrite scaffold files.

The generated `cybersin.yaml` starts with local SQLite storage, a conservative
static cost model, and the container sandbox backend:

```yaml
name: myagent
targets:
  - generic
cost_model:
  cache_similarity_threshold: 0.97
  judge_trigger_band: [0.90, 0.97]
  judge_model: cache-judge
storage:
  backend: sqlite
sandbox:
  backend: docker+gvisor
```

### Local Setup

For live providers and built-in web tools, keep machine-local readiness
settings out of the portable project config:

```sh
cp cybersin.local.example.yaml cybersin.local.yaml
cat > .env <<'EOF'
OPENROUTER_API_KEY=...
TAVILY_API_KEY=...
EOF
```

`cybersin.local.yaml` is gitignored by the scaffold. Store secret
references there as environment-variable names, not raw secrets. A local
config can declare provider/tool availability, sandbox defaults, and
routing/tool permissions:

```yaml
providers:
  openrouter:
    availability: available
    api_key: ${OPENROUTER_API_KEY}
tools:
  tavily:
    availability: auto
    api_key:
      env: TAVILY_API_KEY
defaults:
  provider: openrouter
  model: openai/gpt-4o-mini
permissions:
  routing:
    allowed_providers: [openrouter]
```

Cybersin still honors the existing process environment, and it now loads
an optional project-root `.env` before OpenRouter and Tavily readiness
checks without overriding variables that are already set.

Add prompt sources under `prompts/`, then check and build from inside the
project:

```sh
cybersin check .
cybersin build . --profile dev --frozen
```

Runtime commands discover the project root by walking up from the current
directory until they find `cybersin.yaml`. That means defaults such as
`.cybersin/cybersin.db`, `.cybersin/sandbox`, and `dist/` resolve relative to
the initialized project unless you override them with CLI flags.

### Build and run the sample project

Clone the repository if you want to try the included research-team fixture:

```sh
git clone https://github.com/blokboy/cybersin.git
cd cybersin
```

Check and compile the sample project:

```sh
cybersin check fixtures/ic1-research-team
cybersin build fixtures/ic1-research-team \
  --profile release \
  --frozen
```

Run the compiled project. The runtime automatically creates and uses the SQLite database at `.cybersin/cybersin.db`.

```sh
cybersin \
  --db .cybersin/cybersin.db \
  run \
  --stub \
  --dist fixtures/ic1-research-team/dist \
  --session-id quickstart \
  --agent research-team
```

Inspect the compiled routing, token counts, traces, and observed cost:

```sh
cybersin \
  --db .cybersin/cybersin.db \
  explain researcher fixtures/ic1-research-team \
  --plain
```

Run the sample project's recorded regression suite:

```sh
cybersin eval gate fixtures/ic1-research-team
```

### Interactive shell and help

Run `cybersin` with no arguments from an interactive terminal to open the
Ratatui application shell. The first workflow is prompt conversion: enter
a multiline raw prompt, keep or change the standalone conversion model,
optionally set an output path, and run the same conversion pipeline used
by `cybersin convert`. Live conversion uses OpenRouter and reads its key
from the environment directly or through the project's `.env` and
`cybersin.local.yaml` provider config.

Use `cybersin -help`, `cybersin -h`, or `cybersin --help` to print CLI
help and exit. Bare `cybersin` in a non-interactive context fails clearly
instead of waiting on terminal UI input.

### Live tool execution

Custom tools declared with `run:` and an optional container `image:` are
compiled into `dist/tools.json`; files under the project's `tools/`
directory are packaged into `dist/tools/`. DLQ retries and approved calls
execute those commands in the selected Docker sandbox:

```sh
cybersin \
  --dist fixtures/ic1-research-team/dist \
  --sandbox-backend docker \
  dlq retry '<call-id>'
```

Network egress allowlisting is not implemented yet. A tool whose agent
declares a non-empty `sandbox.egress` fails closed before a container is
started. The `web_search` built-in is recognized but likewise fails
clearly until a search provider is configured.

For the full product design and command surface, see [cybersin-spec.md](cybersin-spec.md).
