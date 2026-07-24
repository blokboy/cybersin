//! Durable supervisor/worker orchestration (spec §8.7).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{RuntimeError, SessionSupervisor, StateRecord, Storage};

pub const DEFAULT_MAX_RESTARTS: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Worker {
    pub id: String,
    pub parent_id: String,
    pub config: Value,
    pub budget_usd: f64,
    pub spent_usd: f64,
    pub restarts: u32,
    pub max_restarts: u32,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Mail {
    pub sender: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkerExit {
    HarnessCrash,
    Failed { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum OrchestrationError {
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Storage(#[from] crate::StorageError),
    #[error(
        "parent budget exceeded: allocated {allocated}, requested {requested}, ceiling {ceiling}"
    )]
    BudgetExceeded {
        allocated: f64,
        requested: f64,
        ceiling: f64,
    },
    #[error("worker {0} not found")]
    WorkerNotFound(String),
    #[error("stale blackboard version: expected {expected:?}, actual {actual:?}")]
    StaleBlackboard {
        expected: Option<i64>,
        actual: Option<i64>,
    },
}

/// Orchestration uses the existing append-only event log as its source of
/// truth. Materialized state is only an index, matching session replay.
pub struct Orchestrator {
    storage: Arc<dyn Storage>,
    supervisor: SessionSupervisor,
    // Serializes compare-and-set within one daemon. Postgres multi-daemon CAS
    // remains a server-mode refinement; stale versions are still explicit.
    blackboard_writes: Mutex<()>,
}

impl Orchestrator {
    pub fn new(storage: Arc<dyn Storage>) -> Self {
        Self {
            supervisor: SessionSupervisor::new(storage.clone()),
            storage,
            blackboard_writes: Mutex::new(()),
        }
    }

    pub fn with_supervisor(storage: Arc<dyn Storage>, supervisor: SessionSupervisor) -> Self {
        Self {
            storage,
            supervisor,
            blackboard_writes: Mutex::new(()),
        }
    }

    pub async fn register_parent(
        &self,
        parent_id: &str,
        agent: &str,
        budget_usd: f64,
    ) -> Result<(), OrchestrationError> {
        self.storage.create_session(parent_id, agent).await?;
        self.storage
            .set_state(
                parent_id,
                "orchestration",
                "budget_usd",
                &serde_json::json!(budget_usd),
            )
            .await?;
        Ok(())
    }

    /// Spawn fixes the allocation permanently. Existing allocations are
    /// reconstructed from spawn events, so a daemon restart cannot reallocate.
    pub async fn spawn(
        &self,
        parent_id: &str,
        child_id: &str,
        config: Value,
        budget_usd: f64,
        max_restarts: Option<u32>,
    ) -> Result<Worker, OrchestrationError> {
        let ceiling = self
            .storage
            .get_state(parent_id, "orchestration", "budget_usd")
            .await?
            .and_then(|r| r.value.as_f64())
            .unwrap_or(0.0);
        let allocated: f64 = self
            .storage
            .load_events(parent_id)
            .await?
            .iter()
            .filter(|e| e.kind == "worker.spawned")
            .filter_map(|e| e.payload["budget_usd"].as_f64())
            .sum();
        if budget_usd < 0.0 || allocated + budget_usd > ceiling + f64::EPSILON {
            return Err(OrchestrationError::BudgetExceeded {
                allocated,
                requested: budget_usd,
                ceiling,
            });
        }
        let worker = Worker {
            id: child_id.into(),
            parent_id: parent_id.into(),
            config,
            budget_usd,
            spent_usd: 0.0,
            restarts: 0,
            max_restarts: max_restarts.unwrap_or(DEFAULT_MAX_RESTARTS),
            status: "running".into(),
        };
        self.storage
            .create_session(
                child_id,
                worker
                    .config
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("worker"),
            )
            .await?;
        self.save_worker(&worker).await?;
        self.storage
            .append_event(parent_id, "worker.spawned", serde_json::to_value(&worker)?)
            .await?;
        Ok(worker)
    }

    pub async fn charge(&self, child_id: &str, usd: f64) -> Result<Worker, OrchestrationError> {
        let mut worker = self.worker(child_id).await?;
        if worker.spent_usd + usd > worker.budget_usd + f64::EPSILON {
            return Err(OrchestrationError::BudgetExceeded {
                allocated: worker.spent_usd,
                requested: usd,
                ceiling: worker.budget_usd,
            });
        }
        worker.spent_usd += usd;
        self.save_worker(&worker).await?;
        Ok(worker)
    }

    pub async fn send(
        &self,
        sender: &str,
        recipient: &str,
        payload: Value,
    ) -> Result<(), OrchestrationError> {
        let signal = format!("mailbox:{sender}");
        self.storage
            .enqueue_signal(
                recipient,
                &signal,
                &serde_json::to_value(Mail {
                    sender: sender.into(),
                    payload,
                })?,
            )
            .await?;
        Ok(())
    }

    /// Drains every queued message from one sender, preserving send order.
    pub async fn drain(
        &self,
        recipient: &str,
        sender: &str,
    ) -> Result<Vec<Mail>, OrchestrationError> {
        let signal = format!("mailbox:{sender}");
        let mut out = Vec::new();
        while let Some(value) = self.storage.take_signal(recipient, &signal).await? {
            out.push(serde_json::from_value(value)?);
        }
        Ok(out)
    }

    pub async fn blackboard_get(
        &self,
        root_id: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<StateRecord>, OrchestrationError> {
        Ok(self
            .storage
            .get_state(root_id, &format!("blackboard:{namespace}"), key)
            .await?)
    }

    pub async fn blackboard_cas(
        &self,
        root_id: &str,
        namespace: &str,
        key: &str,
        expected_version: Option<i64>,
        value: Value,
    ) -> Result<StateRecord, OrchestrationError> {
        let _guard = self.blackboard_writes.lock().await;
        let ns = format!("blackboard:{namespace}");
        let current = self.storage.get_state(root_id, &ns, key).await?;
        let actual = current.as_ref().map(|r| r.updated_seq);
        if actual != expected_version {
            return Err(OrchestrationError::StaleBlackboard {
                expected: expected_version,
                actual,
            });
        }
        self.storage.set_state(root_id, &ns, key, &value).await?;
        Ok(self
            .storage
            .get_state(root_id, &ns, key)
            .await?
            .expect("set state materializes row"))
    }

    /// Only a harness crash is death. It resumes the same child from its
    /// checkpoint (and paired session sandbox snapshot when configured).
    pub async fn worker_exit(
        &self,
        child_id: &str,
        exit: WorkerExit,
    ) -> Result<Worker, OrchestrationError> {
        let mut worker = self.worker(child_id).await?;
        match exit {
            WorkerExit::HarnessCrash if worker.restarts < worker.max_restarts => {
                worker.restarts += 1;
                worker.status = "running".into();
                let config_hash = self
                    .storage
                    .get_session(child_id)
                    .await?
                    .map(|s| s.config_hash)
                    .unwrap_or_default();
                self.supervisor.resume(child_id, &config_hash).await?;
                self.send(child_id, &worker.parent_id, serde_json::json!({"status":"restarted","worker_id":child_id,"restart":worker.restarts})).await?;
            }
            WorkerExit::HarnessCrash => {
                worker.status = "permanently_failed".into();
                self.storage.set_session_status(child_id, "failed").await?;
                self.send(
                    child_id,
                    &worker.parent_id,
                    serde_json::json!({"status":"permanently_failed","worker_id":child_id}),
                )
                .await?;
            }
            WorkerExit::Failed { reason } => {
                worker.status = "failed".into();
                self.storage.set_session_status(child_id, "failed").await?;
                self.send(
                    child_id,
                    &worker.parent_id,
                    serde_json::json!({"status":"failed","worker_id":child_id,"reason":reason}),
                )
                .await?;
            }
        }
        self.save_worker(&worker).await?;
        Ok(worker)
    }

    pub async fn approval_parked(
        &self,
        child_id: &str,
        approval_id: &str,
    ) -> Result<(), OrchestrationError> {
        let mut worker = self.worker(child_id).await?;
        worker.status = "awaiting_approval".into();
        self.storage
            .set_session_status(child_id, "awaiting_approval")
            .await?;
        self.send(child_id, &worker.parent_id, serde_json::json!({"status":"awaiting_approval","worker_id":child_id,"approval_id":approval_id})).await?;
        self.save_worker(&worker).await?;
        Ok(())
    }

    async fn worker(&self, child_id: &str) -> Result<Worker, OrchestrationError> {
        self.storage
            .get_state(child_id, "orchestration", "worker")
            .await?
            .ok_or_else(|| OrchestrationError::WorkerNotFound(child_id.into()))
            .and_then(|r| serde_json::from_value(r.value).map_err(Into::into))
    }

    async fn save_worker(&self, worker: &Worker) -> Result<(), OrchestrationError> {
        self.storage
            .set_state(
                &worker.id,
                "orchestration",
                "worker",
                &serde_json::to_value(worker)?,
            )
            .await?;
        Ok(())
    }
}

impl From<serde_json::Error> for OrchestrationError {
    fn from(value: serde_json::Error) -> Self {
        Self::Storage(crate::StorageError::Json(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStorage;

    async fn fixture() -> (Arc<dyn Storage>, Orchestrator) {
        let storage: Arc<dyn Storage> = Arc::new(SqliteStorage::in_memory().await.unwrap());
        let orchestration = Orchestrator::new(storage.clone());
        orchestration
            .register_parent("parent", "supervisor", 10.0)
            .await
            .unwrap();
        (storage, orchestration)
    }

    #[tokio::test]
    async fn static_child_allocations_and_spend_never_exceed_parent_ceiling() {
        let (_, o) = fixture().await;
        o.spawn("parent", "a", serde_json::json!({"agent":"a"}), 6.0, None)
            .await
            .unwrap();
        o.spawn("parent", "b", serde_json::json!({"agent":"b"}), 4.0, None)
            .await
            .unwrap();
        assert!(matches!(
            o.spawn("parent", "c", Value::Null, 0.01, None).await,
            Err(OrchestrationError::BudgetExceeded { .. })
        ));
        o.charge("a", 6.0).await.unwrap();
        o.charge("b", 4.0).await.unwrap();
        assert!(o.charge("a", 0.01).await.is_err());
    }

    #[tokio::test]
    async fn mailbox_is_sender_addressed_ordered_and_drains_multiple_messages() {
        let (_, o) = fixture().await;
        o.send("a", "parent", serde_json::json!(1)).await.unwrap();
        o.send("b", "parent", serde_json::json!(99)).await.unwrap();
        o.send("a", "parent", serde_json::json!(2)).await.unwrap();
        let mail = o.drain("parent", "a").await.unwrap();
        assert_eq!(
            mail.iter().map(|m| m.payload.clone()).collect::<Vec<_>>(),
            vec![serde_json::json!(1), serde_json::json!(2)]
        );
        assert_eq!(o.drain("parent", "b").await.unwrap().len(), 1);
        assert!(o.drain("parent", "a").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn blackboard_rejects_stale_writers() {
        let (_, o) = fixture().await;
        let first = o
            .blackboard_cas("parent", "research", "answer", None, serde_json::json!(1))
            .await
            .unwrap();
        let second = o
            .blackboard_cas(
                "parent",
                "research",
                "answer",
                Some(first.updated_seq),
                serde_json::json!(2),
            )
            .await
            .unwrap();
        assert_eq!(second.value, serde_json::json!(2));
        assert!(matches!(
            o.blackboard_cas(
                "parent",
                "research",
                "answer",
                Some(first.updated_seq),
                serde_json::json!(3)
            )
            .await,
            Err(OrchestrationError::StaleBlackboard { .. })
        ));
    }

    #[tokio::test]
    async fn crash_resumes_same_worker_but_failures_do_not_restart_and_approval_is_local() {
        let (storage, o) = fixture().await;
        o.spawn(
            "parent",
            "a",
            serde_json::json!({"agent":"a"}),
            5.0,
            Some(1),
        )
        .await
        .unwrap();
        o.spawn("parent", "b", serde_json::json!({"agent":"b"}), 5.0, None)
            .await
            .unwrap();
        storage
            .create_checkpoint("a", Some("before-crash"))
            .await
            .unwrap();
        assert_eq!(
            o.worker_exit("a", WorkerExit::HarnessCrash)
                .await
                .unwrap()
                .restarts,
            1
        );
        assert_eq!(
            o.worker_exit("a", WorkerExit::HarnessCrash)
                .await
                .unwrap()
                .status,
            "permanently_failed"
        );
        assert_eq!(
            o.worker_exit(
                "b",
                WorkerExit::Failed {
                    reason: "budget_halt".into()
                }
            )
            .await
            .unwrap()
            .restarts,
            0
        );
        o.approval_parked("b", "approval-1").await.unwrap();
        assert_eq!(
            storage.get_session("parent").await.unwrap().unwrap().status,
            "running"
        );
        assert_eq!(
            storage.get_session("b").await.unwrap().unwrap().status,
            "awaiting_approval"
        );
        assert!(!o.drain("parent", "b").await.unwrap().is_empty());
    }
}
