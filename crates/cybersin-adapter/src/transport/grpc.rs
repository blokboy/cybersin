//! gRPC transport (spec §10 — "fast path"), via tonic.
//!
//! One bidirectional-streaming RPC (`Adapter/Session`) per session; each
//! `HarnessMessage`/`DaemonMessage` is carried JSON-encoded in the `json`
//! field of the generated `HarnessEnvelope`/`DaemonEnvelope` protobuf
//! messages (see `proto/adapter.proto`). Test/dev code exercises this over
//! a loopback TCP socket (`127.0.0.1:0`, an ephemeral local port) — never
//! a real external network service.

use std::net::SocketAddr;
use std::time::Duration;

use crate::channel::{DaemonChannel, HarnessChannel, TransportError};
use crate::messages::{DaemonMessage, HarnessMessage};
use crate::pb::adapter_client::AdapterClient;
use crate::pb::adapter_server::{Adapter, AdapterServer};
use crate::pb::{DaemonEnvelope, HarnessEnvelope};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

/// Harness-side gRPC channel: the client end of the `Session` bidi stream.
pub struct GrpcHarnessChannel {
    outgoing: mpsc::Sender<HarnessEnvelope>,
    incoming: Streaming<DaemonEnvelope>,
}

#[async_trait]
impl HarnessChannel for GrpcHarnessChannel {
    async fn send(&mut self, msg: HarnessMessage) -> Result<(), TransportError> {
        let json = serde_json::to_string(&msg)?;
        self.outgoing
            .send(HarnessEnvelope { json })
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Option<DaemonMessage> {
        loop {
            match self.incoming.message().await {
                Ok(Some(env)) => {
                    if let Ok(msg) = serde_json::from_str(&env.json) {
                        return Some(msg);
                    }
                    // Malformed envelope payload: not a protocol message
                    // we understand, keep waiting for the next one.
                }
                _ => return None,
            }
        }
    }
}

/// Daemon-side gRPC channel: one connected harness's half of the `Session`
/// bidi stream, as handed to the daemon (or, in this crate's scope, the
/// conformance suite's daemon test double) by [`AdapterService`].
pub struct GrpcDaemonChannel {
    incoming: Streaming<HarnessEnvelope>,
    outgoing: mpsc::Sender<Result<DaemonEnvelope, Status>>,
}

#[async_trait]
impl DaemonChannel for GrpcDaemonChannel {
    async fn send(&mut self, msg: DaemonMessage) -> Result<(), TransportError> {
        let json = serde_json::to_string(&msg)?;
        self.outgoing
            .send(Ok(DaemonEnvelope { json }))
            .await
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Option<HarnessMessage> {
        loop {
            match self.incoming.message().await {
                Ok(Some(env)) => {
                    if let Ok(msg) = serde_json::from_str(&env.json) {
                        return Some(msg);
                    }
                }
                _ => return None,
            }
        }
    }
}

/// The tonic service implementation. Deliberately minimal: its only job is
/// turning each incoming `Session` RPC into a [`GrpcDaemonChannel`] and
/// handing it off over `new_sessions` for whatever plays the daemon role
/// to pick up and drive with real protocol logic. It has no session
/// supervisor / gateway logic of its own — that would be out of scope for
/// this crate (spec §10 is the protocol; the runtime/gateway are separate
/// crates, §13).
#[derive(Clone)]
struct AdapterService {
    new_sessions: mpsc::UnboundedSender<GrpcDaemonChannel>,
}

#[tonic::async_trait]
impl Adapter for AdapterService {
    type SessionStream = std::pin::Pin<
        Box<dyn futures::Stream<Item = Result<DaemonEnvelope, Status>> + Send + 'static>,
    >;

    async fn session(
        &self,
        request: Request<Streaming<HarnessEnvelope>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let incoming = request.into_inner();
        let (tx, rx) = mpsc::channel(32);
        let channel = GrpcDaemonChannel {
            incoming,
            outgoing: tx,
        };
        self.new_sessions.send(channel).map_err(|_| {
            Status::unavailable("daemon side is not currently accepting new sessions")
        })?;
        let out_stream: Self::SessionStream = Box::pin(ReceiverStream::new(rx));
        Ok(Response::new(out_stream))
    }
}

/// Errors accepting the harness's first gRPC session connection
/// (`cybersin run`'s `harness.adapter: grpc` path, issue #37) — distinct
/// from [`TransportError`], which covers an already-established channel.
#[derive(Debug, thiserror::Error)]
pub enum GrpcAcceptError {
    #[error("no harness connected within {0:?}")]
    Timeout(Duration),
    #[error("the gRPC adapter service stopped accepting connections")]
    ServiceStopped,
}

/// Errors connecting to a running [`GrpcAdapterServer`] as a harness.
#[derive(Debug, thiserror::Error)]
pub enum GrpcConnectError {
    #[error("connecting to {0}: {1}")]
    Transport(String, #[source] tonic::transport::Error),
    #[error("opening the Session stream: {0}")]
    Rpc(#[source] Status),
}

/// A running gRPC adapter service (§10's "fast path" transport) bound to
/// one local address. Each connecting harness's `Session` RPC becomes one
/// [`GrpcDaemonChannel`], handed out in connection order via
/// [`GrpcAdapterServer::accept`].
pub struct GrpcAdapterServer {
    addr: SocketAddr,
    sessions: mpsc::UnboundedReceiver<GrpcDaemonChannel>,
}

impl GrpcAdapterServer {
    /// The bound local address — pass this to whatever spawns the harness
    /// process so it knows where to connect (`cybersin run`'s
    /// `harness.adapter: grpc` path uses the `CYBERSIN_ADAPTER_ADDR`
    /// environment variable for this).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Waits up to `timeout` for one harness to connect, returning its
    /// channel. A real caller (unlike this crate's own tests, which know a
    /// client is about to connect) needs a bound wait: a harness process
    /// that hangs or never speaks gRPC must not block `cybersin run`
    /// forever.
    pub async fn accept(
        &mut self,
        timeout: Duration,
    ) -> Result<GrpcDaemonChannel, GrpcAcceptError> {
        tokio::time::timeout(timeout, self.sessions.recv())
            .await
            .map_err(|_| GrpcAcceptError::Timeout(timeout))?
            .ok_or(GrpcAcceptError::ServiceStopped)
    }
}

/// Starts the gRPC adapter service on `bind_addr` (`"127.0.0.1:0"` for an
/// OS-assigned ephemeral port) in a background task and returns a handle
/// to accept connecting harnesses from. No external network exposure is
/// implied by this function itself — callers control `bind_addr`.
pub async fn listen(bind_addr: &str) -> std::io::Result<GrpcAdapterServer> {
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let addr = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);

    let (session_tx, session_rx) = mpsc::unbounded_channel();
    let service = AdapterService {
        new_sessions: session_tx,
    };

    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(AdapterServer::new(service))
            .serve_with_incoming(incoming)
            .await;
    });

    Ok(GrpcAdapterServer {
        addr,
        sessions: session_rx,
    })
}

/// Connects to a running [`GrpcAdapterServer`] as a harness, opening the
/// `Session` bidi stream — the client-side counterpart of
/// [`GrpcAdapterServer::accept`].
pub async fn connect(addr: SocketAddr) -> Result<GrpcHarnessChannel, GrpcConnectError> {
    let mut client = AdapterClient::connect(format!("http://{addr}"))
        .await
        .map_err(|error| GrpcConnectError::Transport(addr.to_string(), error))?;
    let (out_tx, out_rx) = mpsc::channel(32);
    let outbound = ReceiverStream::new(out_rx);
    let response = client
        .session(outbound)
        .await
        .map_err(GrpcConnectError::Rpc)?;
    Ok(GrpcHarnessChannel {
        outgoing: out_tx,
        incoming: response.into_inner(),
    })
}

/// Test/dev helper mirroring `transport::stdio::in_memory_pair`: stands up
/// the gRPC service on an ephemeral loopback port, connects a client, and
/// returns one connected (harness side, daemon side) channel pair. No
/// external network access — `127.0.0.1` only, port 0 (OS-assigned).
pub async fn in_memory_pair() -> (GrpcHarnessChannel, GrpcDaemonChannel) {
    let mut server = listen("127.0.0.1:0")
        .await
        .expect("bind an ephemeral local port for the in-process gRPC test server");
    let harness_side = connect(server.addr())
        .await
        .expect("connect to the in-process gRPC test server");
    let daemon_side = server
        .accept(Duration::from_secs(5))
        .await
        .expect("daemon test double receives the new session handoff");
    (harness_side, daemon_side)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::CallOutcome;
    use serde_json::json;

    #[tokio::test]
    async fn round_trips_a_message_over_loopback_grpc() {
        let (mut harness, mut daemon) = in_memory_pair().await;

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
}
