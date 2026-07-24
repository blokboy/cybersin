# Cybersin

Cybersin is a Rust-based prompt compiler and durable agent runtime that turns typed prompt sources into optimized, routable, cacheable artifacts. It gives agent developers one CLI for deterministic builds, regression evals, sandboxed execution, resumable sessions, cost tracing, and profile-guided optimization.

## Getting Started

### Prerequisites

- Git
- A current stable Rust toolchain (`rustup`, `rustc`, and `cargo`)

Docker is only required for the container-backed sandbox tests; the quickstart below uses the local SQLite runtime.

### Build and run the sample project

Clone the repository and build the workspace:

```sh
git clone https://github.com/blokboy/cybersin.git
cd cybersin
cargo build --workspace
```

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

For the full product design and command surface, see [cybersin-spec.md](cybersin-spec.md).
