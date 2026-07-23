//! Subcommand implementations, one module per top-level `Command` variant
//! (spec §11's CLI surface). Kept as small, independent modules — rather
//! than one large `main.rs` `match` — so a concurrent branch adding
//! different subcommands to this same crate (e.g. the frontend's `build`/
//! `check`, issue #3) only touches `main.rs`'s `Command` enum and its own
//! module, not this issue's command bodies.

pub mod cost;
pub mod run;
pub mod trace;
