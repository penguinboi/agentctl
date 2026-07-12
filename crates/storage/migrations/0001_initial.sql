CREATE TABLE IF NOT EXISTS unified_sessions (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    workspace_path TEXT NOT NULL,
    workspace_fingerprint TEXT NOT NULL,
    active_provider_json TEXT,
    routing_policy TEXT NOT NULL,
    auth_mode_json TEXT NOT NULL,
    status_json TEXT NOT NULL,
    parent_session_id TEXT REFERENCES unified_sessions(id),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    schema_version INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sessions_workspace
    ON unified_sessions(workspace_fingerprint, updated_at DESC);

CREATE TABLE IF NOT EXISTS provider_sessions (
    id TEXT PRIMARY KEY,
    unified_session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    provider_json TEXT NOT NULL,
    native_session_id TEXT NOT NULL,
    native_version TEXT,
    last_synced_seq INTEGER NOT NULL DEFAULT 0 CHECK(last_synced_seq >= 0),
    status_json TEXT NOT NULL,
    reset_at TEXT,
    capabilities_json TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(unified_session_id, provider_json)
);

CREATE TABLE IF NOT EXISTS turns (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    provider_json TEXT,
    prompt_seq INTEGER NOT NULL CHECK(prompt_seq > 0),
    status_json TEXT NOT NULL,
    side_effect_state_json TEXT NOT NULL,
    native_turn_id TEXT,
    continuation INTEGER NOT NULL DEFAULT 0,
    started_at TEXT,
    completed_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS raw_provider_events (
    event_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    turn_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
    provider_json TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL CHECK(seq > 0),
    event_id TEXT NOT NULL UNIQUE,
    turn_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
    origin_provider_json TEXT,
    kind TEXT NOT NULL,
    visibility_json TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    raw_event_id TEXT REFERENCES raw_provider_events(event_id) ON DELETE SET NULL,
    created_at TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    PRIMARY KEY(session_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_events_turn ON events(turn_id, seq);
CREATE INDEX IF NOT EXISTS idx_events_kind ON events(session_id, kind, seq);

CREATE TABLE IF NOT EXISTS sync_receipts (
    provider_session_id TEXT NOT NULL REFERENCES provider_sessions(id) ON DELETE CASCADE,
    canonical_event_id TEXT NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
    projection_version INTEGER NOT NULL CHECK(projection_version >= 0),
    native_receipt TEXT,
    state TEXT NOT NULL DEFAULT 'applied',
    applied_at TEXT NOT NULL,
    PRIMARY KEY(provider_session_id, canonical_event_id, projection_version)
);

CREATE TABLE IF NOT EXISTS provider_health_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT REFERENCES unified_sessions(id) ON DELETE CASCADE,
    provider_json TEXT NOT NULL,
    health_json TEXT NOT NULL,
    checked_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_health_provider_time
    ON provider_health_snapshots(provider_json, checked_at DESC);

CREATE TABLE IF NOT EXISTS workspace_snapshots (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    turn_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
    phase TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    snapshot_json TEXT NOT NULL,
    diff_digest TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS blobs (
    digest TEXT PRIMARY KEY,
    size INTEGER NOT NULL CHECK(size >= 0),
    compression TEXT NOT NULL,
    media_type TEXT NOT NULL,
    redacted INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS context_checkpoints (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    through_seq INTEGER NOT NULL CHECK(through_seq >= 0),
    projection_version INTEGER NOT NULL CHECK(projection_version >= 0),
    checkpoint_json TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(session_id, through_seq, projection_version)
);

CREATE TABLE IF NOT EXISTS routing_decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    decision_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS approvals (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    provider_json TEXT NOT NULL,
    request_json TEXT NOT NULL,
    decision_json TEXT,
    decided_at TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session_branches (
    child_session_id TEXT PRIMARY KEY REFERENCES unified_sessions(id) ON DELETE CASCADE,
    parent_session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    through_seq INTEGER NOT NULL CHECK(through_seq >= 0),
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS protocol_capabilities (
    provider_json TEXT NOT NULL,
    native_version TEXT NOT NULL,
    capability TEXT NOT NULL,
    supported INTEGER NOT NULL,
    evidence_json TEXT NOT NULL,
    probed_at TEXT NOT NULL,
    PRIMARY KEY(provider_json, native_version, capability)
);

-- A short-lived transaction marker allows an explicit local session deletion while
-- keeping direct event/raw-event DELETE statements append-only.
CREATE TABLE IF NOT EXISTS deletion_authorizations (
    session_id TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS workspace_leases (
    workspace_lease_key TEXT PRIMARY KEY,
    owner_pid INTEGER NOT NULL,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    turn_id TEXT REFERENCES turns(id) ON DELETE CASCADE,
    acquired_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    expires_at TEXT
);

CREATE TABLE IF NOT EXISTS processes (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES unified_sessions(id) ON DELETE CASCADE,
    turn_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
    provider_json TEXT,
    pid INTEGER NOT NULL,
    process_group_id INTEGER,
    state TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    started_at TEXT NOT NULL,
    exited_at TEXT
);

CREATE TABLE IF NOT EXISTS plugins (
    name TEXT PRIMARY KEY,
    version TEXT NOT NULL,
    protocol_version INTEGER NOT NULL,
    executable TEXT NOT NULL,
    manifest_json TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TRIGGER IF NOT EXISTS events_no_update
BEFORE UPDATE ON events
WHEN NOT EXISTS (
    SELECT 1 FROM deletion_authorizations WHERE session_id = OLD.session_id
)
BEGIN
    SELECT RAISE(ABORT, 'canonical events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS events_no_delete
BEFORE DELETE ON events
WHEN NOT EXISTS (
    SELECT 1 FROM deletion_authorizations WHERE session_id = OLD.session_id
)
BEGIN
    SELECT RAISE(ABORT, 'canonical events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS raw_events_no_update
BEFORE UPDATE ON raw_provider_events
BEGIN
    SELECT RAISE(ABORT, 'raw provider events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS raw_events_no_delete
BEFORE DELETE ON raw_provider_events
WHEN NOT EXISTS (
    SELECT 1 FROM deletion_authorizations WHERE session_id = OLD.session_id
)
BEGIN
    SELECT RAISE(ABORT, 'raw provider events are append-only');
END;
