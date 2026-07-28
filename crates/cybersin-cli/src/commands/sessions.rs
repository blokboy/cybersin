use std::path::PathBuf;

use clap::Subcommand;
use cybersin_runtime::{materialize_artifact_bundle, DaemonHandle, SessionSupervisor};

use crate::capabilities::{
    execute_sessions_ls, execute_sessions_show, rendered_text, simple_result, SessionsLsInput,
    SessionsShowInput,
};

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
            let execution = execute_sessions_ls(storage.as_ref(), SessionsLsInput { json }).await;
            print!("{}", rendered_text(&execution.events));
            simple_result(&execution.events)
                .unwrap_or_else(|| {
                    Err("sessions ls failed: capability did not emit a terminal event".to_string())
                })
                .map_err(anyhow::Error::msg)?;
        }
        SessionsCommand::Show { session } => {
            let execution =
                execute_sessions_show(storage.as_ref(), SessionsShowInput { session }).await;
            print!("{}", rendered_text(&execution.events));
            simple_result(&execution.events)
                .unwrap_or_else(|| {
                    Err(
                        "sessions show failed: capability did not emit a terminal event"
                            .to_string(),
                    )
                })
                .map_err(anyhow::Error::msg)?;
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
