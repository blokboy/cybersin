# IC-1 research-team fixture

This is the compiler/runtime handoff fixture from issue #9. It deliberately
uses more than the minimal scaffold:

- two prompts with different quality tiers;
- shared fragments and a JSON-schema output contract;
- generic and OpenAI render targets;
- an eval source and an agent configuration with read tools plus a
  critical, approval-gated publishing tool;
- committed `dist/` output for later runtime integration checkpoints.

Issue #14 consumes this committed `dist/` directly in the real daemon and
exercises gateway idempotency, crash/resume, budget degradation, and
approval parking without translating it into the old hand-written fixture
format.

Regenerate the committed artifacts from the repository root:

```sh
cargo run -p cybersin-cli -- build fixtures/ic1-research-team \
  --profile release --frozen
```
