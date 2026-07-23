//! Subcommand implementations, one module per top-level `Command` variant
//! (spec §11's CLI surface). Kept as small, independent modules — rather
//! than one large `main.rs` `match` — so each issue only touches its own
//! module plus the `Command` enum in `main.rs`.

pub mod approval;
pub mod check;
pub mod cost;
pub mod dlq;
pub mod fmt;
pub mod init;
pub mod run;
pub mod trace;
