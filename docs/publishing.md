# Publishing Cybersin

This workspace is prepared so the CLI package publishes as `cybersin` and
installs with:

```sh
cargo install cybersin
```

The `cybersin` package depends on the internal `cybersin-*` crates with both
local `path` entries and registry `version` entries. Local builds use the
workspace paths; packaged crates use the version requirements.

## One-time setup

1. Create or sign in to a crates.io account.
2. Create a crates.io API token.
3. Log Cargo in locally:

```sh
cargo login
```

## Publish order

Publish the leaves first, then the crates that depend on them:

```sh
cargo publish -p cybersin-ir --dry-run
cargo publish -p cybersin-ir

cargo publish -p cybersin-adapter --dry-run
cargo publish -p cybersin-adapter

cargo publish -p cybersin-sandbox --dry-run
cargo publish -p cybersin-sandbox

cargo publish -p cybersin-trace --dry-run
cargo publish -p cybersin-trace

cargo publish -p cybersin-frontend --dry-run
cargo publish -p cybersin-frontend

cargo publish -p cybersin-passes --dry-run
cargo publish -p cybersin-passes

cargo publish -p cybersin-router --dry-run
cargo publish -p cybersin-router

cargo publish -p cybersin-backends --dry-run
cargo publish -p cybersin-backends

cargo publish -p cybersin-runtime --dry-run
cargo publish -p cybersin-runtime

cargo publish -p cybersin-gateway --dry-run
cargo publish -p cybersin-gateway

cargo publish -p cybersin --dry-run
cargo publish -p cybersin
```

After crates.io accepts a crate, allow a short index propagation delay before
publishing crates that depend on it.

## First-release caveat

`cybersin-runtime` must not have a dev-dependency on `cybersin-gateway` in the
published manifest, because `cybersin-gateway` has a normal dependency on
`cybersin-runtime`. Keep gateway-backed approval integration tests in a crate
that already depends on both sides, or run them from a local-only manifest.

## Validation

Before publishing, run:

```sh
cargo check -p cybersin
cargo package -p cybersin --list --allow-dirty
```

Once the internal crates exist on crates.io, this should also pass:

```sh
cargo package -p cybersin --allow-dirty
```
