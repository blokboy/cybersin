#!/usr/bin/env python3
"""Minimal harness adapter for `research-team.agent.yaml` (issue #35 Phase
3's live end-to-end smoke test).

Speaks the newline-JSON adapter protocol (spec §10,
`cybersin_adapter::messages`) directly over its own real stdin/stdout — no
dependency on `cybersin-adapter` itself, since this is Python. One JSON
object per line, each carrying `type`. Scripts the same shape
`cybersin_runtime::stub_agent::run_stub_session` drives against
`StubHarness` in-process: a researcher `llm.request`, a `citation_lookup`
`tool.request`, a synthesizer `llm.request`, then `session.complete`.
"""

import json
import sys

_next_call_id = 0


def _fresh_call_id():
    global _next_call_id
    _next_call_id += 1
    return f"call-{_next_call_id}"


def _send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def _recv():
    line = sys.stdin.readline()
    if not line:
        raise SystemExit("daemon closed the connection unexpectedly")
    return json.loads(line)


def _request(message):
    """Send a call_id-bearing request and wait for its correlated reply."""
    call_id = message["call_id"]
    _send(message)
    while True:
        reply = _recv()
        if reply.get("type") in ("call.result", "call.parked") and reply.get("call_id") == call_id:
            return reply


def main():
    session_start = _recv()
    assert session_start["type"] == "session.start", session_start
    session_id = session_start["session_id"]
    inputs = session_start["inputs"]

    researcher_result = _request(
        {
            "type": "llm.request",
            "call_id": _fresh_call_id(),
            "prompt_name": "researcher",
            "inputs": inputs,
        }
    )
    print(f"loop.py: researcher -> {researcher_result}", file=sys.stderr)

    tool_result = _request(
        {
            "type": "tool.request",
            "call_id": _fresh_call_id(),
            "tool": "citation_lookup",
            "args": {"citation": "C-1"},
        }
    )
    print(f"loop.py: citation_lookup -> {tool_result}", file=sys.stderr)

    synthesizer_result = _request(
        {
            "type": "llm.request",
            "call_id": _fresh_call_id(),
            "prompt_name": "synthesizer",
            "inputs": inputs,
        }
    )
    print(f"loop.py: synthesizer -> {synthesizer_result}", file=sys.stderr)

    _send(
        {
            "type": "session.complete",
            "session_id": session_id,
            "result": {"status": "ok"},
        }
    )


if __name__ == "__main__":
    main()
