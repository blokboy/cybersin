//! A reusable, transport-agnostic scripted harness (issue #41): drives a
//! compiled agent's live session from a YAML script of steps instead of a
//! bespoke per-agent program like `fixtures/ic1-research-team/loop.py`.
//! Built on `cybersin_adapter::stub_harness::StubHarness`, which is
//! already generic over `HarnessChannel` — the same script runs over
//! either transport with no duplicated protocol logic.
//!
//! Transport is auto-detected exactly the way `cybersin run` picks it for
//! its own `harness.adapter: grpc` path: `CYBERSIN_ADAPTER_ADDR` set means
//! gRPC, unset means this process's own stdin/stdout (the process
//! adapter). No flags.
//!
//! Script format — a flat YAML list of steps, run in order after the
//! implicit `session.start`, ending in an implicit `session.complete`:
//!
//! ```yaml
//! - llm_request:
//!     prompt_name: researcher
//!     inputs: { topic: "...", depth: quick, documents: [] }
//! - tool_request:
//!     tool: citation_lookup
//!     args: { citation: "C-1" }
//! ```
//!
//! A step that comes back parked (an `approval: required` tool) prints its
//! approval id and blocks on its eventual resolution — a separate
//! `cybersin approve`/`deny` against the same call id is what unblocks it.
//! Any step that resolves `Failed` stops the script and exits nonzero.

use cybersin_adapter::channel::HarnessChannel;
use cybersin_adapter::messages::CallOutcome;
use cybersin_adapter::stub_harness::{CallOutcomeOrPark, StubHarness};
use cybersin_adapter::transport::{grpc, stdio};
use serde::Deserialize;
use serde_json::Value;

// `serde_yaml`'s default externally-tagged enum representation uses a
// YAML tag (`!llm_request {...}`), not a nested single-key mapping like
// `serde_json`'s — so a plain `#[derive(Deserialize)]` enum can't parse
// the `- llm_request: {...}` shape this script format wants. `untagged`
// with each variant a single-field struct sidesteps the tag entirely and
// matches purely structurally on that one field's name.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Step {
    LlmRequest { llm_request: LlmRequestStep },
    ToolRequest { tool_request: ToolRequestStep },
}

#[derive(Debug, Clone, Deserialize)]
struct LlmRequestStep {
    prompt_name: String,
    inputs: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolRequestStep {
    tool: String,
    args: Value,
}

/// Runs `steps` against an already-connected `harness`: recv the opening
/// `session.start`, execute each step in order, send the closing
/// `session.complete`. The testable core — generic over `HarnessChannel`
/// so tests can drive it against `stdio::in_memory_pair` + `DaemonDouble`
/// without a real process or network, exactly like this crate's own
/// conformance suite does.
async fn run_script<C: HarnessChannel>(
    harness: &mut StubHarness<C>,
    steps: &[Step],
) -> Result<(), String> {
    let (session_id, _inputs, _resume_state) = harness.recv_session_start().await;

    for step in steps {
        let (call_id, outcome) = match step {
            Step::LlmRequest { llm_request } => {
                harness
                    .llm_request(llm_request.prompt_name.clone(), llm_request.inputs.clone())
                    .await
            }
            Step::ToolRequest { tool_request } => {
                harness
                    .tool_request(tool_request.tool.clone(), tool_request.args.clone(), None)
                    .await
            }
        };

        let outcome = match outcome {
            CallOutcomeOrPark::Parked(approval_id) => {
                eprintln!(
                    "scripted_harness: {call_id} parked, awaiting approval {approval_id} \
(run `cybersin approve {approval_id}` or `cybersin deny {approval_id}`)"
                );
                harness.await_result(&call_id).await
            }
            other => other,
        };

        match outcome {
            CallOutcomeOrPark::Result(CallOutcome::Ok { value }) => {
                eprintln!("scripted_harness: {call_id} ok: {value}");
            }
            CallOutcomeOrPark::Result(CallOutcome::Failed { reason, retriable }) => {
                return Err(format!(
                    "{call_id} failed (retriable={retriable}): {reason}"
                ));
            }
            CallOutcomeOrPark::Parked(approval_id) => {
                // `await_result` only returns once a call leaves the
                // parked state, so this can't actually happen — kept as an
                // exhaustive, fail-closed arm rather than an unreachable!.
                return Err(format!(
                    "{call_id} still parked awaiting {approval_id} after await_result"
                ));
            }
            CallOutcomeOrPark::Aborted(reason) => {
                return Err(format!("session aborted: {reason:?}"));
            }
        }
    }

    harness
        .session_complete(session_id, serde_json::json!({"status": "ok"}))
        .await;
    Ok(())
}

fn load_script(path: &str) -> Vec<Step> {
    let contents =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {path}: {error}"));
    serde_yaml::from_str(&contents).unwrap_or_else(|error| panic!("parsing {path}: {error}"))
}

#[tokio::main]
async fn main() {
    let script_path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: scripted_harness <script.yaml>");
        std::process::exit(2);
    });
    let steps = load_script(&script_path);

    let result = match std::env::var("CYBERSIN_ADAPTER_ADDR") {
        Ok(addr) => {
            let addr: std::net::SocketAddr = addr
                .parse()
                .expect("CYBERSIN_ADAPTER_ADDR must be a valid socket address");
            let channel = grpc::connect(addr)
                .await
                .expect("connect to the gRPC adapter server");
            let mut harness = StubHarness::new(channel);
            run_script(&mut harness, &steps).await
        }
        Err(_) => {
            let mut harness = StubHarness::new(stdio::harness_process_io());
            run_script(&mut harness, &steps).await
        }
    };

    if let Err(error) = result {
        eprintln!("scripted_harness: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cybersin_adapter::daemon_double::DaemonDouble;
    use serde_json::json;

    fn script(yaml: &str) -> Vec<Step> {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[tokio::test]
    async fn runs_llm_and_tool_steps_to_completion() {
        let (harness_io, daemon_io) = stdio::in_memory_pair();
        let (mut daemon, _ctrl) = DaemonDouble::new(daemon_io, "sess-1", 100.0);
        daemon.start_session(json!({}), None).await;
        let daemon_task = tokio::spawn(daemon.run());

        let steps = script(
            r#"
- llm_request:
    prompt_name: researcher
    inputs: { topic: "x" }
- tool_request:
    tool: citation_lookup
    args: { citation: "C-1" }
"#,
        );
        let mut harness = StubHarness::new(harness_io);
        run_script(&mut harness, &steps).await.unwrap();

        let summary = daemon_task.await.expect("daemon task join");
        assert!(summary.did_complete());
    }

    #[tokio::test]
    async fn parked_step_blocks_until_approved() {
        let (harness_io, daemon_io) = stdio::in_memory_pair();
        let (daemon, ctrl) = DaemonDouble::new(daemon_io, "sess-approval", 100.0);
        let mut daemon = daemon.require_approval("publish_report");
        daemon.start_session(json!({}), None).await;
        let daemon_task = tokio::spawn(daemon.run());

        let steps = script(
            r#"
- tool_request:
    tool: publish_report
    args: { report: "evidence-backed" }
"#,
        );
        let mut harness = StubHarness::new(harness_io);

        // `run_script` blocks on `await_result` once parked, so drive it
        // concurrently with the approval instead of awaiting it first.
        let run = tokio::spawn(async move {
            run_script(&mut harness, &steps).await.unwrap();
        });

        // Give the park a moment to land, then resolve it — mirrors a
        // separate `cybersin approve <call-id>` process. `DaemonDouble`
        // assigns approval ids sequentially ("approval-1", "approval-2",
        // ...) starting from its own counter, not the real `ToolGateway`'s
        // "{tool}:{idem_key}" scheme — deterministic here since this is
        // the only parked call in this daemon instance.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ctrl.approve("approval-1");

        run.await.expect("scripted run task panicked");
        let summary = daemon_task.await.expect("daemon task join");
        assert!(summary.did_complete());
    }

    #[tokio::test]
    async fn a_failed_step_stops_the_script_and_reports_the_reason() {
        let (harness_io, daemon_io) = stdio::in_memory_pair();
        let (daemon, ctrl) = DaemonDouble::new(daemon_io, "sess-fail", 100.0);
        let mut daemon = daemon.require_approval("send_email");
        daemon.start_session(json!({}), None).await;
        let daemon_task = tokio::spawn(daemon.run());

        let steps = script(
            r#"
- tool_request:
    tool: send_email
    args: {}
"#,
        );
        let mut harness = StubHarness::new(harness_io);

        let run = tokio::spawn(async move { run_script(&mut harness, &steps).await });

        // `cybersin deny <call-id>` — same deterministic approval id
        // reasoning as the approval test above.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ctrl.deny("approval-1", "denied");

        let error = run
            .await
            .expect("scripted run task panicked")
            .expect_err("a denied call must fail the script");
        assert!(error.contains("denied"));

        let _ = daemon_task.await;
    }
}
