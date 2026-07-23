//! One module per `cybersin` subcommand implemented so far. Each `run`
//! function returns `Ok(Some(summary))` to print on success, `Ok(None)`
//! for a command that already printed everything it needs to, or
//! `Err(message)` for a clear failure the CLI prints to stderr before
//! exiting nonzero.

pub mod check;
pub mod fmt;
pub mod init;
