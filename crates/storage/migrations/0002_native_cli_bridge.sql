CREATE TABLE IF NOT EXISTS native_launches (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    provider_json TEXT NOT NULL,
    native_session_id TEXT NOT NULL,
    workspace_lease_key TEXT NOT NULL,
    child_pid INTEGER,
    state TEXT NOT NULL CHECK(state IN ('started', 'capture_ready', 'exited', 'uncertain', 'captured', 'failed')),
    exit_code INTEGER,
    error TEXT,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    started_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_native_launches_open
    ON native_launches(session_id, state, started_at);

CREATE UNIQUE INDEX IF NOT EXISTS idx_native_launches_one_writer
    ON native_launches(workspace_lease_key)
    WHERE state IN ('started', 'capture_ready', 'exited', 'uncertain');

CREATE TABLE IF NOT EXISTS native_handoffs (
    launch_id TEXT PRIMARY KEY REFERENCES native_launches(id) ON DELETE CASCADE,
    provider_session_id TEXT NOT NULL REFERENCES provider_sessions(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    native_session_id TEXT NOT NULL,
    through_seq INTEGER NOT NULL CHECK(through_seq >= 0),
    capsule TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('staged', 'delivering', 'delivered', 'uncertain')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_native_handoffs_session_state
    ON native_handoffs(session_id, state, created_at);

CREATE INDEX IF NOT EXISTS idx_turns_session_status
    ON turns(session_id, status_json, updated_at);

CREATE INDEX IF NOT EXISTS idx_events_native_hook_base
    ON events(session_id, json_extract(payload_json, '$.native_hook.base_key'))
    WHERE json_extract(payload_json, '$.native_hook.base_key') IS NOT NULL;
