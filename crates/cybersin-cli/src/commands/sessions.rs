use std::path::PathBuf;

use clap::Subcommand;
use cybersin_runtime::{DaemonHandle, SessionSupervisor};

#[derive(Debug, Subcommand)]
pub enum SessionsCommand {
    Ls,
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
}

pub async fn execute(db: PathBuf, command: SessionsCommand) -> anyhow::Result<()> {
    let daemon = DaemonHandle::auto_start(db).await?;
    let storage = daemon.storage();
    match command {
        SessionsCommand::Ls => {
            for s in storage.list_sessions().await? {
                println!(
                    "{}\t{}\t{}\t{}",
                    s.session_id, s.status, s.agent_name, s.config_hash
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
    }
    Ok(())
}
