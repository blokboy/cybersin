//! `cybersin-gateway`'s unified error type.

use crate::schema::SchemaError;

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("schema validation failed: {0}")]
    Schema(#[from] SchemaError),
    #[error(transparent)]
    Storage(#[from] cybersin_runtime::StorageError),
    #[error("no tool call {0:?}")]
    NotFound(String),
    #[error("tool call {0:?} is not a dead letter (not failed, or already dropped)")]
    NotADeadLetter(String),
    #[error("tool call {0:?} is not awaiting approval")]
    NotAwaitingApproval(String),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
