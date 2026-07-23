//! Adapter protocol transports (spec §10): newline-JSON over stdio
//! (universal) and gRPC via tonic (fast path). Both implement the same
//! [`crate::channel::HarnessChannel`] / [`crate::channel::DaemonChannel`]
//! traits so protocol logic is transport-agnostic.

pub mod grpc;
pub mod stdio;
