use std::path::PathBuf;

use clap::Subcommand;
use cybersin_runtime::{materialize_artifact_bundle, DaemonHandle, SessionSupervisor};

use crate::session_liveness::{display_liveness, heartbeat_display, holder_display, now_unix_ms};

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    Ls {
        #[arg(long)]
        json: bool,
    },
    Show {
        session: String,
    },
    Resume {
        session: String,
        #[arg(long)]
        config_hash: String,
    },
    Kill {
        session: String,
    },
    Migrate {
        session: String,
        #[arg(long)]
        config_hash: String,
    },
    Materialize {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        config_hash: Option<String>,
        #[arg(long)]
        out: PathBuf,
    },
}

pub async fn execute(db: PathBuf, command: SessionsCommand) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(db).await?;
    let storage = daemon.storage();
    match command {
        SessionsCommand::Ls { json } => {
            let sessions = storage.list_sessions().await?;
            let now = now_unix_ms();
            if json {
                let rows: Vec<_> = sessions
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "session_id": s.session_id,
                            "status": s.status,
                            "agent_name": s.agent_name,
                            "config_hash": s.config_hash,
                            "created_unix_ms": s.created_unix_ms,
                            "last_heartbeat_unix_ms": s.last_heartbeat_unix_ms,
                            "heartbeat_holder": s.heartbeat_holder,
                            "liveness": display_liveness(s, now),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            for s in sessions {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    s.session_id,
                    s.status,
                    s.agent_name,
                    s.config_hash,
                    display_liveness(&s, now),
                    heartbeat_display(&s),
                    holder_display(&s)
                );
            }
        }
        SessionsCommand::Show { session } => {
            let s = storage
                .get_session(&session)
                .await?
                .ok_or_else(|| anyhow::anyhow!("session {session:?} not found"))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "session_id": s.session_id, "agent_name": s.agent_name, "status": s.status,
                    "config_hash": s.config_hash, "created_unix_ms": s.created_unix_ms,
                    "last_heartbeat_unix_ms": s.last_heartbeat_unix_ms,
                    "heartbeat_holder": s.heartbeat_holder,
                    "liveness": display_liveness(&s, now_unix_ms()),
                    "events": storage.load_events(&session).await?,
                    "state": storage.list_state(&session).await?,
                    "checkpoint": storage.latest_checkpoint(&session).await?,
                }))?
            );
        }
        SessionsCommand::Resume {
            session,
            config_hash,
        } => {
            let state = SessionSupervisor::new(storage)
                .resume(&session, &config_hash)
                .await?;
            println!(
                "resumed {session}\n{}",
                serde_json::to_string_pretty(&state)?
            );
        }
        SessionsCommand::Kill { session } => {
            SessionSupervisor::new(storage).kill(&session).await?;
            println!("killed {session}");
        }
        SessionsCommand::Migrate {
            session,
            config_hash,
        } => {
            storage.migrate_session(&session, &config_hash).await?;
            println!("migrated {session} to {config_hash}");
        }
        SessionsCommand::Materialize {
            session,
            config_hash,
            out,
        } => {
            let config_hash = match (session, config_hash) {
                (Some(session), None) => {
                    storage
                        .get_session(&session)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("session {session:?} not found"))?
                        .config_hash
                }
                (None, Some(config_hash)) => config_hash,
                (Some(_), Some(_)) => {
                    anyhow::bail!("pass either --session or --config-hash, not both")
                }
                (None, None) => anyhow::bail!("pass --session or --config-hash"),
            };
            let count = materialize_artifact_bundle(storage.as_ref(), &config_hash, &out).await?;
            println!(
                "materialized {count} files for {config_hash} to {}",
                out.display()
            );
        }
    }
    Ok(())
}
