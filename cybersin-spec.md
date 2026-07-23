# Cybersin — Unified Product Spec

**Status:** Draft v2.3 (v1 scope expanded: orchestration pulled in from v2, §15 open questions resolved)
**Name:** a play on Project Cybersyn — Stafford Beer's 1971 cybernetic control room for the Chilean economy: real-time telemetry flowing up, regulatory decisions flowing down, a human in the loop at the point of judgment. The same shape as this system: traces flow up from runtime, optimized decisions compile down, approvals gate the irreversible. The one-letter twist keeps the homage while making the name ours — and reads as a wink at what agents get up to without a gateway watching.

**Thesis:** one product, one CLI, one language, hard internal boundaries. Cybersin is a prompt **compiler** and an agent **runtime** in a single Rust binary — the way cargo is a build system, resolver, and test runner without visible seams.

## 1. Product summary

Cybersin treats prompts as programs and agents as their execution. It compiles human-authored prompt sources into optimized, routable, cacheable artifacts, then executes agents against them with durability, sandboxing, idempotent tool calls, observability, and cost governance.

**One sentence:** `cybersin build && cybersin run` takes a developer from "agent idea" to a traced, sandboxed, budget-capped, resumable agent — and every week of production traces makes the next build cheaper.

**One binary, two roles:** `cybersin` (CLI, stateless) and `cybersind` (daemon, auto-started on first runtime command, owns all state). Compile commands are pure functions and never touch the daemon.

## 2. Goals

- Model-portable prompt authoring: write once, render idiomatically per model family.
- Cost optimization as an automated, reviewable compiler pass — routing, caching, cascades, and context budgets decided at build time from a cost model.
- Deterministic builds: same sources + lockfile → byte-identical artifacts; model-assisted passes pinned in the lockfile.
- Safety by default: every tool call through the gateway, every generated-code execution sandboxed, every session budgeted.
- Durable by default: sessions checkpoint and resume without repeating side effects.
- Observable by default: OTel spans with USD cost as a first-class attribute; traces feed profile-guided re-optimization.
- Local-first: SQLite + Docker on a laptop; Postgres + Firecracker via config, same binary.
- Harness-agnostic: any agent loop integrates via a small adapter protocol.

## 3. Non-goals (v1)

- Hosted SaaS, web UI, prompt marketplace.
- General workflow engine for non-agent jobs.
- Fine-tuning/training.
- Multi-tenancy and org quotas (named v2 scope, §12).
- A public artifact contract for third-party runtimes (internal format is extraction-ready; stabilized only if a real external consumer appears).

## 4. Project coverage map

| Project | Where it lives | Status |
|---|---|---|
| 1. Instrumented harness | Trace & cost core (§8.5) | v1 |
| 2. Semantic cache + router | Router pass (§6.3) + executor (§8.3) | v1 |
| 3. Idempotent tool gateway | Gateway (§8.2) | v1 |
| 4. Sandboxed execution | Sandbox service (§8.4) | v1 |
| 5. Durable runtime | Session supervisor (§8.1) | v1 |
| 6. Context compiler | Budget pass (§6.2) + context assembler (§8.3a) | v1 |
| 7. Eval + regression CI | Eval compilation (§6.4) + runner/gate (§8.6) | v1 |
| 8. Multi-agent orchestration | Session supervisor extension (§8.7) | v1 |
| 9. Platform (self-serve, tenancy) | Templates + doctor in v1; tenancy/quotas (§12.1) | partial v1 |

## 5. Source language & project layout

```
myagent/
  cybersin.yaml          # project config: targets, cost model (incl. cold-start cache/judge thresholds), storage, sandbox backend
  cybersin.lock          # pinned models, prices, model-assisted pass outputs
  prompts/*.prompt.yaml
  fragments/             # shared includes: tone, schemas, rubrics
  evals/*.eval.yaml
  agents/*.agent.yaml    # runtime config: harness, tools, budgets, sandbox policy
  dist/                  # build output (committed in v1 for reviewability)
```

### 5.1 Prompt sources (`*.prompt.yaml`)

```yaml
name: researcher
quality: high
inputs: { topic: string, depth: enum[quick, thorough], documents: list[document] }
tools: [web_search, web_fetch]
sections:
  - id: role
    priority: 100
    body: |
      You are a research analyst...
  - id: instructions
    priority: 90
    body: !include fragments/research-method.md
  - id: documents
    priority: 50
    body: "{{#each documents}}...{{/each}}"
output_contract: { type: json_schema, schema: !include fragments/schemas/report.json }
```

Sections-with-priorities are the unit of budget eviction, cache-key granularity, and provider prefix-cache alignment. Typed inputs are validated at build time and at every runtime render.

### 5.2 Eval sources (`*.eval.yaml`)

Cases with typed inputs and assertions (`json_valid`, `judge` with rubric + min score, `contains_none`, custom). `runs_per_case` yields score distributions, never point estimates. Scope is single-prompt output quality — a rubric/judge vocabulary has no shape for deterministic runtime invariants (did a crashed worker resume correctly, did spend respect a budget ceiling); those are conformance-scenario concerns (§8.7, §10), not eval cases. An individual orchestration worker's own prompt calls can still have ordinary eval coverage like any other prompt.

### 5.3 Agent config (`*.agent.yaml`)

```yaml
name: research-agent
harness: { adapter: process, command: ["python", "loop.py"] }
budget: { usd_per_session: 2.50, on_breach: degrade }
tools:
  - { name: send_email, class: critical, guards: [deny_external_domains], approval: required }
  - { name: web_fetch, class: read }
sandbox: { scope: call, egress: [api.example.com], limits: { cpu: 1, mem_mb: 512, wall_s: 120 } }
```

## 6. Compile pipeline

```
sources ─► Frontend ─► IR ─► Optimizer ─► Router ─► Backends ─► dist/
```

### 6.1 Frontend
Parse, resolve `!include` graph (cycle detection), typecheck inputs against template usage, emit IR v1 as `cybersin-ir` types (direct descendant of IR schema v0).

### 6.2 Optimizer (IR→IR passes)
- `lint-fast` — structural checks answerable straight off the IR (unused inputs); runs first, before any paid pass, so a broken prompt fails before `compress` spends anything.
- `dedupe` — shared fragments collapse to refs.
- `compress` — opt-in model-assisted token reduction under a preserve-behavior constraint; outputs pinned in the lockfile, verified by eval gate.
- `reorder` — stable sections first for provider prefix caching.
- `budget` — per-target eviction plans: which sections drop, in what order, at which context sizes. *(Project 6, compile half.)*
- `lint` — checks that depend on other passes' output: contradictions, dead sections (evicted at every target's budget size, so they never reach a model — only knowable after `budget` runs).

Each pass is a `Pass` trait impl over IR; the pipeline is data (a `Vec<Box<dyn Pass>>` per build profile), so `--profile dev` skips compression by construction. `lint-fast` and `lint` are separate `Pass` impls, split by real dependency (IR-only vs. post-`budget`) rather than by convention, and both run in every profile since neither is paid.

### 6.3 Router (compile-time cost minimization)
Inputs: quality tiers, lockfile prices, latency requirements, observed trace statistics when present (§9). Output `routing.json`: per prompt, an ordered decision list — cache lookup (thresholds, judge-tier config) → cascade steps with confidence rubrics → provider fallbacks. Cache and judge are pseudo-models with ~zero cost, making routing one uniform optimization. Judge-tier calls bill to the requesting session's own budget, like any other routing step — no separate platform ledger exists in v1.

**Cold start:** cache-similarity threshold and judge-trigger band have no observed-trace input on a project's first build, so they're declared static defaults in `cybersin.yaml`'s cost model (scaffolded by `cybersin init` with conservative starting values — biased toward false cache-misses, never false cache-hits, since a wrong cache hit is silently wrong rather than loudly expensive). Caching and cascading are live from build #1 using these declared numbers; `cybersin optimize` later tightens or loosens them from observed data through the same PR-reviewed report as any other PGO change (§9). *(Project 2, compile half.)*

### 6.4 Eval compilation
Eval sources compile against prompt IR (input types checked, rubrics resolved) into executable suites. *(Project 7, compile half.)*

### 6.5 Backends
Per model family: idiomatic rendering (tag style, tool-schema dialect, message split) + constraint validation, behind a `Backend` trait. `--target generic` retained for portability.

### 6.6 `dist/` (internal format)
`manifest.json` (build hash, git SHA, lockfile hash, per-artifact content hashes), `prompts/`, `routing.json`, `cache.json` (with `namespace_version` — the cache-invalidation signal), `evals/`, `budget/`. Internal but on-disk and diff-friendly: reviewable PRs and reproducible builds keep their value; no cross-version skew handling because compiler and runtime ship in one binary and share `cybersin-ir` types with serde on both sides.

## 7. Determinism & lockfile

`cybersin.lock` pins model identifiers, prices, embedding model, and every model-assisted pass output (keyed by input hash). `cybersin build --frozen` fails if any pass would need a network call — the CI mode. A PR therefore shows exactly which compressed rewrite or price update changed.

## 8. Runtime (`cybersind`)

Event-sourced session supervisor at the core; every subsystem hangs off session events. Storage behind a `Storage` trait: SQLite (dev) and Postgres (server) via sqlx, no ORM.

### 8.1 Session supervisor & durability *(Project 5)*
- Session = one agent execution: append-only `events`, typed state namespaces, checkpoints (pre-side-effect + periodic; also snapshots the sandbox when `sandbox.scope: session`, §8.4).
- Resume: replay events against the pinned config hash; tool calls found `succeeded` in the ledger return memoized results. All nondeterminism (time, random, LLM responses) flows through recorded events for replay determinism.
- Primitives: `state.get/set`, `checkpoint`, `sleep(duration)`, `signal.wait` — with `cybersin notify <session> <payload>` for mid-flight steering — and `spawn` (§8.7, orchestration).
- Sessions pin `agent_hash` + build hash; config changes require explicit `sessions migrate`.

### 8.2 Tool gateway *(Project 3)*
- All tool calls pass through `cybersind`: schema validation, then the idempotency ledger — `tool_calls` UNIQUE `(tool, idem_key)`, states `pending → succeeded | failed`, DB constraint wins races. Keys auto-derived (`session:seq`) unless supplied. A denied approval resolves to `failed(reason: denied)` — a distinct terminal outcome from a transient execution failure, so it's auditable in traces and isn't treated as retriable by `dlq retry`.
- Retry classes: `read` (retry freely), `write` (retry with key), `critical` (never auto-retry).
- Dead-letter queue + `cybersin dlq ls|show|retry|drop`.
- Policy hooks: rate limits, declarative argument guards, approval gates — flagged calls park the session (`awaiting_approval`), resume on `cybersin approve <call-id>`. Durability makes multi-day approval waits free. `cybersin deny <call-id>` does **not** kill the session: it delivers `failed(denied)` to the harness through the normal tool-result channel — same path any `failed` result takes — and the agent's own logic decides what happens next (revise and re-request approval, try another tool, end itself). Critical class's "never auto-retry" already stops the runtime from silently resubmitting a denied call; the gateway's job ends at recording the human's decision durably, not at deciding whether the session survives it.

### 8.3 Route/cache executor *(Project 2, runtime half)*
Executes `routing.json`/`cache.json` verbatim: hash lookup → vector kNN (sqlite-vec / pgvector) → borderline judge tier → cascade with confidence checks → fallbacks. Obeys `namespace_version` invalidation; per-call `bypass`. Every decision (hit/miss/escalation, similarity, judge outcome) lands in span attributes — PGO raw material.

### 8.3a Context assembler *(Project 6, runtime half)*
At each `llm.request`, assembles the final context from the compiled prompt + live inputs (retrieved documents, memory, conversation) by executing the compiled budget plan: fills sections in priority order, evicts per plan when over the target's token budget, records what was dropped as span attributes. Compile time decides the *policy*; the assembler applies it to data that only exists at call time.

### 8.4 Sandbox service *(Project 4)*
Ephemeral sandbox for agent-generated code, scoped per-agent via `sandbox.scope: call | session` (default `call`). Backends behind a `SandboxBackend` trait: `docker` (dev), `docker+gvisor` (default), `firecracker` (v1.1). Cybersin shells out to container runtimes rather than embedding them. Default-deny egress with per-agent allowlists; CPU/mem/pids/wall-clock hard limits; read-only base + copy-on-write workspace; snapshot/diff/restore. Killed-by-limit is a first-class, inspectable outcome.

`scope: call` (default) gives every call a fresh workspace, discarded after; cross-call data must flow through `state.get/set` or the blackboard (§8.7), both already event-sourced and resume-safe. `scope: session` persists one COW workspace across a session's calls — for this mode, every session checkpoint (§8.1) also takes a sandbox snapshot tied to the same checkpoint ID, since the idempotency ledger's "don't re-run a succeeded call" guarantee says nothing about whether that call's filesystem side effects survived a crash; resume restores the paired snapshot before replaying events forward.

### 8.5 Trace & cost core *(Project 1)*
OTel-compatible span per LLM call, tool call, sandbox exec, cache decision; attributes include tokens, **usd_cost**, model, cache status, retries, evicted sections. Queryable offline: `cybersin trace ls|show`, `cybersin cost --by session|agent|model|tool|day`. Budgets enforced by the executor; on breach: `halt` | `degrade` (cheapest cascade step) | `ask` (approval gate). OTLP export optional.

### 8.6 Eval runner *(Project 7, runtime half)*
`cybersin eval run` (N-run distributions, live or recorded providers), `cybersin eval gate` (nonzero exit for CI), `cybersin trace sample --to-eval` (promote production traces to fixtures — the production→test flywheel). `eval gate` defaults to recorded-provider mode in CI; live-provider runs trigger on two events — automatically whenever `cybersin lock update` changes a pinned model or price (deliberate drift), and on a weekly timer regardless of lockfile changes (silent provider-side drift under a stable pin).

### 8.7 Orchestration *(Project 8)*
`spawn(child_config)` over the durable runtime: supervisor/worker trees, mailbox messaging, shared blackboard namespaces, budget propagation.
- **Budget:** a parent's USD ceiling divides among children as a static weighted split fixed at spawn time (`spawn(config, budget_usd: N)`); no dynamic reallocation in v1 (reclaimable pooling deferred, §12.2).
- **Mailbox:** a queue-based primitive, distinct from `signal.wait`, built on the same event-sourced delivery mechanism `signal` uses — every send/receive is a recorded, replayable event. Unlike `signal.wait`'s single-pending-nudge shape, mailboxes are addressable per sender and drain multiple queued messages.
- **Blackboard:** shared typed namespaces visible across a supervisor/worker tree, using optimistic CAS — versioned writes where a stale write fails and the caller retries — the same DB-constraint-as-referee pattern the gateway's idempotency ledger (§8.2) already uses.
- **Worker death & reassignment:** only a harness process crash counts as "death"; budget breaches and critical-tool failures are the child completing in a failed state, surfaced to the parent via mailbox, not death. Reassignment means resume-from-checkpoint via the existing session resume path (§8.1), not a fresh respawn — the resumed child continues drawing from its already-allocated weighted budget, and for `sandbox.scope: session` workers its sandbox is restored from the paired checkpoint snapshot (§8.4) before replay continues. Bounded by `max_restarts` (default 3, checked before budget); exceeding it reports the child as permanently failed via mailbox.
- **Approvals:** a child's `awaiting_approval` parks only that child; parent and siblings keep running and learn of the block via mailbox. Tree-wide propagation to ancestor status is deferred (§12.2, maybe).
- Adapter protocol messages for spawn/mailbox extend the protocol at M6 (v0→v1), designed against the real runtime rather than speculatively at M0 (§10).
- Orchestration correctness (crash/resume, budget-respects-ceiling, mailbox delivery) is verified by conformance scenarios (§10), not `eval gate` — those are deterministic runtime invariants, not the output-quality claims eval's rubric/judge vocabulary is built for (§5.2). `eval gate` (Project 7) stays scoped to single-prompt quality regression; it has no dependency on Project 8's own test coverage.

Exit criterion: supervisor survives worker death, reassigns, total spend respects the parent budget.

## 9. Profile-guided optimization

`cybersin optimize [--since 7d | --traces file.jsonl]` reads the daemon's trace store directly (file mode kept for CI/portability) and re-runs the router and cache-threshold computation with observed values: real cost/latency distributions, cascade escalation rates, borderline-judge outcomes, eval-scored production samples. Output: a normal build + `optimize-report.md` naming each change and its evidence ("raised cache threshold 0.90→0.93: 14% of hits in that band were judge-rejected"). PGO changes always ship through PR review in v1.

## 10. Adapter protocol (harness plugin interface)

Versioned with the product, documented in-repo, conformance-tested. Transport: newline-JSON over stdio (universal) or gRPC via tonic (fast path). Harness → daemon: `llm.request {prompt_name, inputs}` (names a *prompt*, never a model — routing, caching, budget, and context assembly apply transparently), `tool.request`, `state.*`, `checkpoint`, `sleep`, `signal.wait`, `session.complete`. Daemon → harness: `session.start {inputs, resume_state?}`, `signal.delivered`, `session.abort`. Conformance scenarios: resume mid-task, double-fire, budget breach, parked approval. Spawn and mailbox message types extend the protocol at M6, once orchestration's runtime semantics exist to design against, with their own conformance scenarios (child crash → parent resumes it, mailbox double-delivery) rather than being speculatively baked into the v0 surface at M0. Official adapters: Python, TypeScript, Rust (the Rust one is just `cybersin-adapter`, sharing types with the daemon).

## 11. CLI surface (v1)

```
cybersin init | fmt | check | build [--frozen|--watch] | diff <ref> | explain <prompt>
cybersin lock update
cybersin run <agent.yaml> [--input f]
cybersin sessions ls|show|resume|kill|migrate
cybersin notify <session> <json>
cybersin approve <call-id> | deny <call-id>
cybersin trace ls|show|export | cost --by <dim>
cybersin dlq ls|show|retry|drop
cybersin sandbox exec|snapshot|diff|restore
cybersin eval run|gate
cybersin optimize [--since t | --traces f]
cybersin daemon [--server]         # TCP+mTLS multi-worker mode
cybersin doctor                    # env, backends, build/runtime consistency
```

`cybersin explain` is the flagship TUI (ratatui): per-prompt pipeline decisions, per-section tokens per target, routing tree, estimated cost — extended with *observed* cost side-by-side once traces exist. The operations views (`sessions`, `trace`, `cost`) are the control-room half of the namesake.

## 12. v2 scope (named, not vanished)

### 12.1 Platform tier *(Project 9 remainder)*
Multi-tenant `cybersind --server`: per-team namespaces and quotas, RBAC on approvals, org cost dashboards, `cybersin new --template` golden paths. Exit criterion: someone outside the project ships an agent in <30 minutes without talking to you. (v1 carries the seeds: `init` scaffolding, `doctor`, server mode.)

### 12.2 Orchestration refinements (deferred)
Two v1 orchestration decisions (§8.7) were deliberately simplified with a named v2 upgrade path:
- **Dynamic, reclaimable budget pooling** — children draw against a shared pool and return unspent budget to siblings, instead of v1's static weighted split. Revisit once static allocation is shown to actually pinch in practice.
- **Approval-pending propagation to parent** — a child's `awaiting_approval` surfacing as ancestor status in `cybersin sessions ls`, instead of v1's local-park-only. A maybe, not a commitment — `sessions ls` filtered by parent-id may already cover the need.

### 12.3 Extraction hatch
Compiler and runtime stay in separate crates, and `dist/` stays a real on-disk format. If an external consumer or producer materializes, stabilizing the contract and splitting the products is a refactor, not a rewrite — done then, with a real user's requirements.

## 13. Rust commitment & workspace

All-Rust is now a decision, not an open question. Rationale on record:
- **Shared types end-to-end:** `cybersin-ir` structs serialize to `dist/` and deserialize in the executor with the same serde definitions — no cross-language contract, no codegen, no skew between the product's own halves.
- **The compiler is the distinctive core**, and pass pipelines want Rust: exhaustive enum matching over IR nodes makes "did every pass handle the new section kind?" a compile error, not a production bug.
- **The daemon has no Go-exclusive need:** tokio + sqlx + tonic cover event sourcing, storage, and RPC; sandbox control is `std::process` shelling to container runtimes.
- **TUI:** ratatui is the strongest terminal UI ecosystem in any language right now, and `explain` + the control-room views are the product's face.
- **Continuity:** shares toolchain with existing Rust work (PyO3 experience transfers to the Python adapter if it ever needs a native core).

Workspace:

```
cybersin/
  crates/
    cybersin-ir/         # IR types + serde; zero heavy deps; the internal contract
    cybersin-frontend/   # parse, includes, typecheck
    cybersin-passes/     # Pass trait + lint-fast/dedupe/compress/reorder/budget/lint
    cybersin-router/     # cost model, routing/cache emission, PGO recompute
    cybersin-backends/   # Backend trait + per-family renderers
    cybersin-runtime/    # session supervisor, events, checkpoints, resume
    cybersin-gateway/    # ledger, retry classes, DLQ, policies, approvals
    cybersin-sandbox/    # SandboxBackend trait + docker/gvisor/firecracker
    cybersin-trace/      # span store, cost rollups, OTLP + JSONL export
    cybersin-adapter/    # protocol types + stdio/gRPC servers + conformance
    cybersin-cli/        # clap commands + ratatui views; the `cybersin` binary
```

Dependency discipline: `cybersin-ir` depends on serde only; runtime crates depend on `ir` but never on `frontend`/`passes` (the executor consumes artifacts, not sources); `cli` is the only crate that sees everything. `cargo deny` + a CI check on the dependency graph keep the extraction hatch real.

Key dependencies: tokio, sqlx (sqlite + postgres features), tonic/prost, ratatui + crossterm, clap, serde/serde_yaml/serde_json, sqlite-vec bindings, opentelemetry-rust, minijinja (chosen for §5.1 templating — a maintained, Jinja-compatible parser over a hand-rolled handlebars subset, consistent with this build's role as a best-practices benchmark rather than a from-scratch learning exercise).

## 14. Milestones

| # | Deliverable | Exit criterion |
|---|---|---|
| M0 | Adapter protocol v0 + conformance scenarios | scenarios runnable against a stub |
| M1 | `cybersin-ir` + frontend + `check`; daemon skeleton + trace core | stub agent runs on a hand-written dist/; costs visible in `cybersin cost` |
| M2 | Backends + `build --frozen`; gateway + ledger + DLQ | byte-identical rebuilds; chaos double-fire → zero duplicate side effects |
| M3 | Router + cache emission; executor + context assembler | replayed workload shows measured $ saved; evictions visible in spans |
| M4 | Sandbox service (docker+gvisor) | fork bomb + exfil contained, logged, session survives |
| M5 | Durable sessions + resume + notify; budgets + approvals | kill -9 mid-task → resume repeats zero calls; breach degrades; critical call parks/resumes |
| M6 | Orchestration: `spawn` + supervisor/worker trees + budget propagation (§8.7) | supervisor survives worker death, reassigns, total spend respects the parent budget |
| M7 | Eval compile + run + gate; `explain` TUI | seeded regression caught in CI |
| M8 | `optimize` PGO loop | a week of traces measurably improves routing, with report |
| M9 | Polish: doctor, docs, templates, quickstart | a stranger ships an agent in <30 min |

## 15. Resolved decisions (v2.3)

Formerly "open questions" (v2.2); all resolved this pass.

1. **Template language:** minijinja (§13) — maintained Jinja-compatible parser, chosen over hand-rolling a handlebars subset given this build's role as a best-practices benchmark.
2. **`dist/` committed to git** (§6.6) — stays committed; it's the diff surface that makes §8.6/§9's weekly PGO PRs reviewable, not just generated build noise.
3. **Judge-tier billing:** the requesting session's own budget (§6.3) — no platform ledger exists in v1 absent a tenancy system for one to belong to.
4. **`eval gate` live-run cadence** (§8.6): recorded-provider by default in CI; live runs trigger on `cybersin lock update` (deliberate model/price change) *and* a weekly timer (silent provider-side drift under a stable pin).
5. **sqlite-vec maturity gate at M3** — concrete go/no-go checklist, fail any one → fall back to brute-force kNN in-process (§8.3):
   - Static-linkable: `cargo build` works with no separately-installed C toolchain step.
   - Cross-platform: builds and passes its test suite on macOS (dev) and Linux (CI/server).
   - Incremental upsert: supports inserting/updating individual vectors, not bulk-load-only.
   - Concurrent read safety under tokio, no external locking required.
   - Latency: p99 kNN query under 10ms at 50k vectors on ordinary dev hardware.
