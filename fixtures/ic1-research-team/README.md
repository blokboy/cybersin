# IC-1 research-team fixture

This is the compiler/runtime handoff fixture from issue #9. It deliberately
uses more than the minimal scaffold:

- two prompts with different quality tiers;
- shared fragments and a JSON-schema output contract;
- generic and OpenAI render targets;
- an eval source and an agent configuration;
- committed `dist/` output for later runtime integration checkpoints.

Regenerate the committed artifacts from the repository root:

```sh
cargo run -p cybersin-cli -- build fixtures/ic1-research-team \
  --profile release --frozen
```
