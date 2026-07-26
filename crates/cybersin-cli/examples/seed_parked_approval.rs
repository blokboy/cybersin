//! Manual-testing helper for `cybersin ops`'s Approvals tab (issue #52):
//! seeds a call parked behind an approval gate directly against a
//! project's `Storage`, without needing a real compiled `dist/` or a
//! live agent session. Not part of the product's own runtime — a
//! developer convenience for exercising the Approvals tab by hand.
//!
//! Usage:
//!   cargo run -p cybersin-cli --example seed_parked_approval -- <db-path> <session-id> <tool> <idem-key-seed>
//!
//! Then point `cybersin ops` at the same `--db` to see (and
//! approve/deny) the parked row.

use std::path::PathBuf;
use std::sync::Arc;

use cybersin_gateway::{ApprovalGate, EchoExecutor, RetryClass, ToolGateway};
use cybersin_runtime::DaemonHandle;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let db = PathBuf::from(
        args.next()
            .expect("usage: <db-path> <session-id> <tool> <idem-key-seed>"),
    );
    let session_id = args.next().expect("session id");
    let tool = args.next().expect("tool name");
    let seed = args.next().expect("idem key seed");

    let daemon = DaemonHandle::auto_start(&db).await.unwrap();
    daemon
        .storage()
        .create_session(&session_id, "manual-check-agent")
        .await
        .unwrap();
    let gateway = ToolGateway::new(daemon.storage(), Arc::new(EchoExecutor))
        .with_policy_hook(Arc::new(ApprovalGate::for_tools([tool.as_str()])));
    let outcome = gateway
        .call(
            &session_id,
            &tool,
            serde_json::json!({"amount": 10_000}),
            Some(seed.clone()),
            RetryClass::Write,
        )
        .await
        .unwrap();
    println!("seeded {tool}:{seed} -> {outcome:?}");
}
