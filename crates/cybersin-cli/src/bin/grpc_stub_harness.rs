//! Minimal gRPC-speaking test harness (issue #37): connects to the daemon
//! address given by `CYBERSIN_ADAPTER_ADDR`, waits for `session.start`,
//! and immediately replies `session.complete`. Exists so
//! `tests/run_live_grpc.rs` can exercise `cybersin run`'s
//! `harness.adapter: grpc` path end-to-end without a Python gRPC client in
//! this repo — the stdio path's fixture tests already cover the full
//! agent loop (llm/tool requests), so this only needs to prove the
//! transport wiring itself works.

use cybersin_adapter::channel::HarnessChannel;
use cybersin_adapter::messages::{DaemonMessage, HarnessMessage};
use cybersin_adapter::transport::grpc;

#[tokio::main]
async fn main() {
    let addr = std::env::var("CYBERSIN_ADAPTER_ADDR")
        .expect("CYBERSIN_ADAPTER_ADDR must be set (cybersin run's harness.adapter: grpc path)")
        .parse()
        .expect("CYBERSIN_ADAPTER_ADDR must be a valid socket address");

    let mut channel = grpc::connect(addr)
        .await
        .expect("connect to the gRPC adapter server");

    let session_id = match channel.recv().await {
        Some(DaemonMessage::SessionStart { session_id, .. }) => session_id,
        other => panic!("expected session.start, got {other:?}"),
    };

    channel
        .send(HarnessMessage::SessionComplete {
            session_id,
            result: serde_json::json!({"status": "ok"}),
        })
        .await
        .expect("send session.complete");

    // `send().await` resolving only means the message was accepted into
    // the local mpsc buffer feeding tonic's request stream — unlike the
    // stdio transport, where a completed `write()` to a pipe is durable
    // in the kernel even if the writer exits immediately after, gRPC's
    // actual network flush happens on a background task inside this same
    // process's tokio runtime. Returning from `main()` immediately races
    // that flush: the runtime can shut down before the bytes ever reach
    // the daemon. A brief pause gives it a chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}
