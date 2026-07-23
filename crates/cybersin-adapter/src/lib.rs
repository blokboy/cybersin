//! `cybersin-adapter` — the harness↔daemon adapter protocol (spec §10).
//!
//! This crate is the protocol layer only: message types, the stdio and
//! gRPC transports that carry them, and a conformance scenario suite any
//! harness adapter implementation (Python, TypeScript, or this crate's own
//! Rust reference client) must pass. It does not implement the real
//! session supervisor, tool gateway, or budget engine — [`daemon_double`]
//! is the minimum daemon-side test logic needed to drive the conformance
//! scenarios, not a real `cybersin-runtime`/`cybersin-gateway`.
//!
//! # Layout
//! - [`messages`] — `HarnessMessage` / `DaemonMessage`, the wire protocol.
//! - [`channel`] — transport-agnostic `HarnessChannel` / `DaemonChannel`.
//! - [`transport::stdio`] — newline-JSON over any `AsyncRead`/`AsyncWrite`.
//! - [`transport::grpc`] — tonic bidi-streaming service carrying the same
//!   JSON messages.
//! - [`stub_harness`] — a minimal, scriptable reference harness used to
//!   exercise the conformance scenarios.
//! - [`daemon_double`] — the minimum daemon-side test double the
//!   conformance scenarios drive the stub harness against.

pub mod channel;
pub mod daemon_double;
pub mod messages;
pub mod stub_harness;
pub mod transport;

mod pb;
