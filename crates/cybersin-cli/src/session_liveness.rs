use cybersin_runtime::{
    heartbeat_liveness_at, is_terminal_session_status, HeartbeatLiveness, SessionRecord,
    DEFAULT_HEARTBEAT_STALE_AFTER_MS,
};

pub(crate) fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) fn display_liveness(session: &SessionRecord, now_unix_ms: i64) -> &'static str {
    if is_terminal_session_status(&session.status) {
        return "terminal";
    }
    match heartbeat_liveness_at(session, now_unix_ms, DEFAULT_HEARTBEAT_STALE_AFTER_MS) {
        HeartbeatLiveness::Fresh => "live",
        HeartbeatLiveness::Stale => "stale",
        HeartbeatLiveness::None => "none",
    }
}

pub(crate) fn heartbeat_display(session: &SessionRecord) -> String {
    session
        .last_heartbeat_unix_ms
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

pub(crate) fn holder_display(session: &SessionRecord) -> &str {
    session.heartbeat_holder.as_deref().unwrap_or("-")
}
