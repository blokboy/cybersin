use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use crate::{RuntimeError, Storage};

/// Durable lifecycle operations shared by the daemon and CLI.
pub struct SessionSupervisor {
    storage: Arc<dyn Storage>,
}

impl SessionSupervisor {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self { storage }
    }

    pub async fn resume(&self, session_id: &str, config_hash: &str) -> Result<Value, RuntimeError> {
        let session =
            self.storage.get_session(session_id).await?.ok_or_else(|| {
                RuntimeError::Session(format!("session {session_id:?} not found"))
            })?;
        if session.config_hash != config_hash {
            return Err(RuntimeError::Session(format!(
                "config hash mismatch: session pins {:?}, requested {:?}; run `cybersin sessions migrate`",
                session.config_hash, config_hash
            )));
        }
        let checkpoint = self.storage.latest_checkpoint(session_id).await?;
        // Rebuild the public resume state from the append-only log. The
        // materialized `session_state` table is only an index; events are
        // the source of truth.
        let mut namespaces: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
        for event in self.storage.load_events(session_id).await? {
            if event.kind == "state.set" {
                if let (Some(namespace), Some(key), Some(value)) = (
                    event.payload["namespace"].as_str(),
                    event.payload["key"].as_str(),
                    event.payload.get("value"),
                ) {
                    namespaces
                        .entry(namespace.into())
                        .or_default()
                        .insert(key.into(), value.clone());
                }
            }
        }
        let replayed_state =
            serde_json::to_value(namespaces).map_err(crate::storage::StorageError::from)?;
        self.storage
            .set_session_status(session_id, "running")
            .await?;
        self.storage
            .append_event(
                session_id,
                "session.resumed",
                serde_json::json!({
                    "config_hash": config_hash,
                    "checkpoint_id": checkpoint.as_ref().map(|c| c.checkpoint_id)
                }),
            )
            .await?;
        Ok(replayed_state)
    }

    pub async fn kill(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.storage
            .set_session_status(session_id, "killed")
            .await?;
        self.storage
            .append_event(session_id, "session.killed", Value::Null)
            .await?;
        Ok(())
    }

    /// Records a nondeterministic value once and replays it by stable key thereafter.
    pub async fn recorded_value<F>(
        &self,
        session_id: &str,
        key: &str,
        produce: F,
    ) -> Result<Value, RuntimeError>
    where
        F: FnOnce() -> Value,
    {
        let kind = format!("nondeterminism.{key}");
        if let Some(event) = self
            .storage
            .load_events(session_id)
            .await?
            .into_iter()
            .find(|e| e.kind == kind)
        {
            return Ok(event.payload["value"].clone());
        }
        let value = produce();
        self.storage
            .append_event(session_id, &kind, serde_json::json!({"value": value}))
            .await?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStorage;

    #[tokio::test]
    async fn resume_requires_pinned_hash_until_explicit_migration() {
        let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
        storage
            .create_session_pinned("s", "a", "old")
            .await
            .unwrap();
        let supervisor = SessionSupervisor::new(storage.clone());
        assert!(supervisor
            .resume("s", "new")
            .await
            .unwrap_err()
            .to_string()
            .contains("sessions migrate"));
        storage.migrate_session("s", "new").await.unwrap();
        storage
            .set_state("s", "memory", "answer", &serde_json::json!(42))
            .await
            .unwrap();
        let state = supervisor.resume("s", "new").await.unwrap();
        assert_eq!(state["memory"]["answer"], 42);
    }

    #[tokio::test]
    async fn nondeterministic_values_are_recorded_then_replayed() {
        let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
        storage.create_session("s", "a").await.unwrap();
        let supervisor = SessionSupervisor::new(storage);
        let first = supervisor
            .recorded_value("s", "random", || serde_json::json!(42))
            .await
            .unwrap();
        let replay = supervisor
            .recorded_value("s", "random", || serde_json::json!(99))
            .await
            .unwrap();
        assert_eq!(first, replay);
    }
}
