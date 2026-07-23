//! stdio transport: newline-delimited JSON (spec §10 — "universal").
//!
//! One [`HarnessMessage`]/[`DaemonMessage`] per line. Generic over any
//! `AsyncRead`/`AsyncWrite` pair so production code can wire it to a real
//! process's stdin/stdout (see [`harness_process_io`]) while tests wire it
//! to an in-memory `tokio::io::duplex` pipe — no real process, no network.
//!
//! Harness-side and daemon-side are distinct wrapper types
//! ([`StdioHarnessChannel`], [`StdioDaemonChannel`]), each implementing
//! exactly one of [`HarnessChannel`]/[`DaemonChannel`], rather than one
//! type implementing both — the two directions genuinely disagree on
//! which message type is "outgoing" vs "incoming", and a single type
//! implementing both traits makes `.send(...)`/`.recv()` ambiguous at
//! every call site.

use crate::channel::{DaemonChannel, HarnessChannel, TransportError};
use crate::messages::{DaemonMessage, HarnessMessage};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Shared newline-JSON read/write plumbing, wrapped by both
/// [`StdioHarnessChannel`] and [`StdioDaemonChannel`].
struct RawStdio<R, W> {
    reader: BufReader<R>,
    writer: W,
    line_buf: String,
}

impl<R, W> RawStdio<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            line_buf: String::new(),
        }
    }

    async fn write_line<T: serde::Serialize>(&mut self, msg: &T) -> Result<(), TransportError> {
        let mut json = serde_json::to_string(msg)?;
        json.push('\n');
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn read_line<T: serde::de::DeserializeOwned>(&mut self) -> Option<T> {
        self.line_buf.clear();
        loop {
            let n = self.reader.read_line(&mut self.line_buf).await.ok()?;
            if n == 0 {
                return None; // EOF
            }
            let trimmed = self.line_buf.trim();
            if trimmed.is_empty() {
                self.line_buf.clear();
                continue; // tolerate blank lines between messages
            }
            return serde_json::from_str(trimmed).ok();
        }
    }
}

/// The harness side of a stdio session: sends [`HarnessMessage`]s, reads
/// [`DaemonMessage`]s.
pub struct StdioHarnessChannel<R, W>(RawStdio<R, W>);

impl<R, W> StdioHarnessChannel<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self(RawStdio::new(reader, writer))
    }
}

#[async_trait]
impl<R, W> HarnessChannel for StdioHarnessChannel<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, msg: HarnessMessage) -> Result<(), TransportError> {
        self.0.write_line(&msg).await
    }

    async fn recv(&mut self) -> Option<DaemonMessage> {
        self.0.read_line().await
    }
}

/// The daemon side of a stdio session: sends [`DaemonMessage`]s, reads
/// [`HarnessMessage`]s.
pub struct StdioDaemonChannel<R, W>(RawStdio<R, W>);

impl<R, W> StdioDaemonChannel<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    pub fn new(reader: R, writer: W) -> Self {
        Self(RawStdio::new(reader, writer))
    }
}

#[async_trait]
impl<R, W> DaemonChannel for StdioDaemonChannel<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn send(&mut self, msg: DaemonMessage) -> Result<(), TransportError> {
        self.0.write_line(&msg).await
    }

    async fn recv(&mut self) -> Option<HarnessMessage> {
        self.0.read_line().await
    }
}

/// Production entry point: a harness adapter process's real stdin/stdout,
/// as the daemon spawns it (`harness: { adapter: process, command: [...] }`,
/// spec §5.3).
pub fn harness_process_io() -> StdioHarnessChannel<tokio::io::Stdin, tokio::io::Stdout> {
    StdioHarnessChannel::new(tokio::io::stdin(), tokio::io::stdout())
}

/// Test/dev helper: an in-memory, in-process pair of stdio channels — one
/// harness-side, one daemon-side — connected by `tokio::io::duplex` pipes.
/// No process spawn, no real stdio, no network.
pub fn in_memory_pair() -> (
    StdioHarnessChannel<tokio::io::DuplexStream, tokio::io::DuplexStream>,
    StdioDaemonChannel<tokio::io::DuplexStream, tokio::io::DuplexStream>,
) {
    let (h_read, d_write) = tokio::io::duplex(64 * 1024);
    let (d_read, h_write) = tokio::io::duplex(64 * 1024);
    let harness_side = StdioHarnessChannel::new(h_read, h_write);
    let daemon_side = StdioDaemonChannel::new(d_read, d_write);
    (harness_side, daemon_side)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::CallOutcome;
    use serde_json::json;

    #[tokio::test]
    async fn round_trips_a_message_over_in_memory_pipes() {
        let (mut harness, mut daemon) = in_memory_pair();

        harness
            .send(HarnessMessage::LlmRequest {
                call_id: "c1".into(),
                prompt_name: "researcher".into(),
                inputs: json!({"topic": "x"}),
            })
            .await
            .unwrap();

        let received = daemon.recv().await.unwrap();
        assert_eq!(
            received,
            HarnessMessage::LlmRequest {
                call_id: "c1".into(),
                prompt_name: "researcher".into(),
                inputs: json!({"topic": "x"}),
            }
        );

        daemon
            .send(DaemonMessage::CallResult {
                call_id: "c1".into(),
                outcome: CallOutcome::Ok { value: json!("ok") },
            })
            .await
            .unwrap();

        let reply = harness.recv().await.unwrap();
        assert_eq!(
            reply,
            DaemonMessage::CallResult {
                call_id: "c1".into(),
                outcome: CallOutcome::Ok { value: json!("ok") },
            }
        );
    }

    #[tokio::test]
    async fn recv_returns_none_on_close() {
        let (harness, daemon) = in_memory_pair();
        drop(harness);
        let mut daemon = daemon;
        assert!(daemon.recv().await.is_none());
    }
}
