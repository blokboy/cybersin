//! Transport-agnostic channel traits.
//!
//! A harness adapter talks to the daemon through a [`HarnessChannel`]; the
//! daemon (or, in the conformance suite, the daemon test double) talks to
//! one connected harness through a [`DaemonChannel`]. Both the stdio
//! transport (`transport::stdio`) and the gRPC transport
//! (`transport::grpc`) implement these same two traits, so protocol logic
//! (the stub harness, the conformance daemon double, the scenario tests
//! themselves) is written once and exercised against both transports.

use crate::messages::{DaemonMessage, HarnessMessage};
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("failed to encode message as JSON: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("the peer's channel is closed")]
    Closed,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport error: {0}")]
    Other(String),
}

/// The harness-side half of a session: send requests to the daemon,
/// receive pushes/replies from it.
#[async_trait]
pub trait HarnessChannel: Send {
    async fn send(&mut self, msg: HarnessMessage) -> Result<(), TransportError>;

    /// Returns `None` once the daemon has closed the channel.
    async fn recv(&mut self) -> Option<DaemonMessage>;
}

/// The daemon-side half of a session: receive requests from one connected
/// harness, send it replies/pushes.
#[async_trait]
pub trait DaemonChannel: Send {
    async fn send(&mut self, msg: DaemonMessage) -> Result<(), TransportError>;

    /// Returns `None` once the harness has closed the channel.
    async fn recv(&mut self) -> Option<HarnessMessage>;
}
