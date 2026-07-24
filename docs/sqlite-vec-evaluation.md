# sqlite-vec maturity gate

Issue #15 requires sqlite-vec to pass every item in the §8.3 go/no-go
checklist. This repository does not link sqlite-vec, carry its native build
integration, or run a 50,000-vector benchmark. The gate therefore fails
closed and the route executor uses its in-process brute-force cosine kNN
implementation.

| Criterion | Result | Repository evidence |
| --- | --- | --- |
| Static-linkable without a separately installed C toolchain step | Fail | No sqlite-vec dependency or build integration is present. |
| Builds and passes on macOS and Linux | Fail | No sqlite-vec cross-platform CI coverage is present. |
| Incremental upsert | Fail | sqlite-vec behavior is not integrated or tested. |
| Concurrent-safe under Tokio without external locking | Fail | sqlite-vec behavior is not integrated or tested. |
| p99 below 10 ms at 50,000 vectors | Fail | No qualifying benchmark is present. |

The fallback is deterministic, requires no external library, performs exact
hash lookup before a linear cosine scan, and supports incremental replacement
by `(prompt_name, input_hash)`. `SQLITE_VEC_EVALUATION` exposes the gate result
to runtime callers and tests.
