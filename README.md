# Cybersin

Cybersin is a Rust-based prompt compiler and durable agent runtime that turns typed prompt sources into optimized, routable, cacheable artifacts. It gives agent developers one CLI for deterministic builds, regression evals, sandboxed execution, resumable sessions, cost tracing, and profile-guided optimization.

![Cybersin crate architecture](docs/assets/cybersin-crate-architecture.svg)

![Cybersin build and run data flow](docs/assets/cybersin-build-and-run-dataflow.svg)

## Getting Started

### Prerequisites

- Git
- A current stable Rust toolchain (`rustup`, `rustc`, and `cargo`)

Docker is only required for the container-backed sandbox tests; the quickstart below uses the local SQLite runtime.

### Create a project spine

Clone the repository and build the workspace:

```sh
git clone https://github.com/blokboy/cybersin.git
cd cybersin
cargo build --workspace
```

Create the basic Cybersin project spine:

```sh
./target/debug/cybersin init myagent
```

Plain `init` is safe for an existing codebase. It creates core project
files (`cybersin.yaml`, `cybersin.lock`, `cybersin.local.example.yaml`,
and `.gitignore`) plus empty `prompts/`, `fragments/`, `evals/`,
`agents/`, and `tools/` directories. It does not create starter prompt
sources, evals, agents, harness files, sample inputs, or `dist/`.
Existing files are skipped by default and reported in the command output;
use `--dry-run` to preview or `--force` to overwrite scaffold files.

For live providers and built-in web tools, keep machine-local readiness
settings out of the portable project config:

```sh
cp myagent/cybersin.local.example.yaml myagent/cybersin.local.yaml
cat > myagent/.env <<'EOF'
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

Add prompt sources under `myagent/prompts/`, then check and build:

```sh
./target/debug/cybersin check myagent
./target/debug/cybersin build myagent --profile dev --frozen
```

### Build and run the sample project

Check and compile the included research-team project:

```sh
./target/debug/cybersin check fixtures/ic1-research-team
./target/debug/cybersin build fixtures/ic1-research-team \
  --profile release \
  --frozen
```

Run the compiled project. The runtime automatically creates and uses the SQLite database at `.cybersin/cybersin.db`.

```sh
./target/debug/cybersin \
  --db .cybersin/cybersin.db \
  run \
  --stub \
  --dist fixtures/ic1-research-team/dist \
  --session-id quickstart \
  --agent research-team
```

Inspect the compiled routing, token counts, traces, and observed cost:

```sh
./target/debug/cybersin \
  --db .cybersin/cybersin.db \
  explain researcher fixtures/ic1-research-team \
  --plain
```

Run the sample project's recorded regression suite:

```sh
./target/debug/cybersin eval gate fixtures/ic1-research-team
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
./target/debug/cybersin \
  --dist fixtures/ic1-research-team/dist \
  --sandbox-backend docker \
  dlq retry '<call-id>'
```

Network egress allowlisting is not implemented yet. A tool whose agent
declares a non-empty `sandbox.egress` fails closed before a container is
started. The `web_search` built-in is recognized but likewise fails
clearly until a search provider is configured.

For the full product design and command surface, see [cybersin-spec.md](cybersin-spec.md).
