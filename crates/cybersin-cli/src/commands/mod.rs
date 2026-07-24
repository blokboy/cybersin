//! Subcommand implementations, one module per top-level `Command` variant
//! (spec §11's CLI surface). Kept as small, independent modules — rather
//! than one large `main.rs` `match` — so each issue only touches its own
//! module plus the `Command` enum in `main.rs`.

pub mod approval;
pub mod build;
pub mod check;
pub mod cost;
pub mod daemon;
pub mod diff;
pub mod dlq;
pub mod eval;
pub mod fmt;
pub mod init;
pub mod notify;
pub mod run;
pub mod sandbox;
pub mod sessions;
pub mod trace;
