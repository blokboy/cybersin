use std::path::PathBuf;

use cybersin_runtime::DaemonHandle;

pub async fn execute(db: PathBuf, session: String, payload: String) -> anyhow::Result<()> {
    let value: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|e| anyhow::anyhow!("payload must be valid JSON: {e}"))?;
    let signal = value
        .get("signal")
        .and_then(|v| v.as_str())
        .unwrap_or("notify");
    DaemonHandle::auto_start(db)
        .await?
        .storage()
        .enqueue_signal(&session, signal, &value)
        .await?;
    println!("notified {session}");
    Ok(())
}
