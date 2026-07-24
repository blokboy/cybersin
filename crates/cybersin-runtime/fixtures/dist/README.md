# Hand-written stub `dist/` fixture

This directory is **hand-authored**, not compiler output — `cybersin-frontend`,
`cybersin-passes`, `cybersin-router`, and `cybersin-backends` don't exist yet
(they're later issues). It exists to satisfy spec §14's M1 exit criterion:
"stub agent runs on a hand-written `dist/`; costs visible in `cybersin cost`".

Layout mirrors spec §6.6's real `dist/` shape, simplified to what
`cybersin-runtime`'s stub agent (`src/stub_agent.rs`) actually reads:

- `manifest.json` — build/git identifying fields only (no lockfile/content
  hashes — nothing here needs to verify byte-identical rebuilds yet).
- `prompts/researcher.json` — a real `cybersin_ir::PromptIr`, by hand.
  Three sections (`role` 100, `instructions` 90, `documents` 50) sized in
  round numbers of whitespace-separated words (10 / 12 / 19) so the token
  math in `src/session.rs`'s `estimate_tokens`/`assemble_context` is easy
  to hand-verify: 10+12+19 = 41 total tokens.
- `routing.json` — **not** the real router's ordered
  cache/cascade/fallback decision list (spec §6.3/§8.3, `cybersin-router`'s
  job) — just enough pricing (`usd_per_1k_*_tokens`, a fixed
  `completion_tokens_estimate`) for the stub daemon to compute a
  `usd_cost` per call.
- `budget/researcher.json` — a real `cybersin_ir::BudgetArtifact`, sized
  deliberately small (`context_window_tokens: 40`,
  `reserved_output_tokens: 10` → 30 tokens available) so the 41-token
  prompt above trips eviction: dropping the 19-token `documents` section
  brings it to 22, under budget. This is what makes the stub run's spans
  show a non-empty `evicted_sections` without needing a giant fixture.
- `tools.json` (issue #13, spec §8.2) — optional per-tool approval-gate
  policy. Only `wire_transfer` is gated here (`retry_class: critical`,
  `approval: required`) — deliberately a tool name the fixed stub-agent
  script (`src/stub_agent.rs`) never calls, so the existing end-to-end
  scenario's span count/costs are unaffected; budget/approval tests drive
  `wire_transfer` themselves.
- `cascade.json` (issue #13, spec §8.5) — optional cheapest-first fallback
  `RoutingEntry` list per prompt, consulted on a `usd_per_session` budget
  breach with `on_breach: degrade`. `researcher`'s cascade adds one
  cheaper step (`gpt-4o-nano`) ahead of `routing.json`'s `gpt-4o-mini`.

Changing the section wording changes the token math above — keep the
word counts in sync with this file's comments (or just run
`cargo test -p cybersin-runtime` and let `dist::tests` /
`session::tests` catch the drift).
