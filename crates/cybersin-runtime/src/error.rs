//! `cybersin-runtime`'s unified error type. Every fallible seam in this
//! crate (storage, trace, dist-fixture loading, adapter transport) funnels
//! into `RuntimeError` so callers (the CLI, tests) have one error type to
//! match on rather than threading four crates' error types by hand.

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{0}")]
    Session(String),
    #[error(transparent)]
    Storage(#[from] crate::storage::StorageError),
    #[error(transparent)]
    Trace(#[from] cybersin_trace::TraceError),
    #[error(transparent)]
    Dist(#[from] crate::dist::DistError),
    #[error("adapter transport error: {0}")]
    Transport(#[from] cybersin_adapter::channel::TransportError),
    #[error("sqlite error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("session task panicked: {0}")]
    Join(String),
    #[error("TLS configuration error: {0}")]
    Tls(String),
}
