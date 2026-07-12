//! Concrete local operations shared by the native bridge command dispatcher.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

use agentctl_core::{
    AgentEvent, CanonicalEvent, EventId, EventVisibility, NativeEffectStatus, NativeSession,
    NativeTranscript, NativeTranscriptItem, ProviderHealth, ProviderKind, ProviderStatus,
    SessionStatus, SideEffectState, TurnId, TurnStatus, UnifiedSession, UnifiedSessionId,
    UsageSnapshot,
};
use agentctl_plugin_protocol::{CURRENT_PROTOCOL_VERSION, PluginCapabilities, PluginManifest};
use agentctl_storage::{
    AgentctlStore, BlobRef, ContextCheckpointRecord, NativeLaunchRecord, NativeLaunchState,
    ProviderSessionRecord, RawProviderEvent, SqliteStore, TurnRecord, canonical_content_hash,
};
use agentctl_telemetry::{PayloadGuard, PayloadLimits, RedactionConfig, Redactor};
use agentctl_transcript::{
    CompactionPolicy, ContextCheckpoint, ExportBlob, build_export, compact, read_jsonl, write_jsonl,
};
use agentctl_workspace::{GitSnapshot, WorkspaceIdentity, WorkspaceLease, capture_git_snapshot};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::paths::AgentctlPaths;

const MAX_IMPORT_FILE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;
const PLUGIN_MANIFEST_FILE: &str = "plugin.toml";

pub fn open_store(paths: &AgentctlPaths) -> Result<AgentctlStore> {
    AgentctlStore::open(&paths.database, &paths.blobs).context("failed to open local state")
}

#[derive(Clone, Debug)]
pub struct NativeLaunchWorkspaceSnapshots {
    pub before: GitSnapshot,
    pub after: Option<GitSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeFailoverContinuation {
    pub event_id: EventId,
    pub seq: u64,
    pub turn_id: Option<TurnId>,
    pub side_effect_state: SideEffectState,
}

/// Closes provider turns left operationally active by a dead native process.
///
/// `Failed` is the terminal execution disposition; uncertainty remains
/// explicit in the monotonic side-effect state and in an idempotent audit
/// event. The audit event is appended first so a crash between the two writes
/// leaves the turn recoverable on the next boot rather than silently closed.
pub fn terminalize_native_crash_turns(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    launch: &NativeLaunchRecord,
) -> Result<Vec<TurnId>> {
    ensure!(
        launch.session_id == session.id,
        "native crash turn recovery crossed canonical sessions"
    );
    let mut recovered = Vec::new();
    for turn in store.recovery_candidates_for(session.id, &launch.provider)? {
        let effects = turn.side_effect_state.observe(SideEffectState::Possible);
        let event_id = deterministic_native_crash_turn_event_id(session.id, launch.id, turn.id);
        if let Some(existing) = store.event_by_id(event_id)? {
            ensure!(
                existing.session_id == session.id
                    && existing.turn_id == Some(turn.id)
                    && existing.kind == "native_turn_crash_closed"
                    && existing
                        .payload
                        .get("source_launch_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(launch.id.to_string().as_str()),
                "deterministic native crash closure event id collision"
            );
        } else {
            let payload = guard.process_json(&serde_json::json!({
                "source_launch_id": launch.id,
                "source_provider": launch.provider,
                "previous_status": turn.status,
                "terminal_status": TurnStatus::Failed,
                "side_effect_state": effects,
                "uncertainty_preserved": true,
                "replay_allowed": false,
            }))?;
            let event = CanonicalEvent {
                schema_version: 1,
                session_id: session.id,
                seq: 0,
                event_id,
                turn_id: Some(turn.id),
                origin_provider: Some(launch.provider.clone()),
                kind: "native_turn_crash_closed".to_owned(),
                visibility: EventVisibility::Internal,
                content_hash: canonical_content_hash(
                    "native_turn_crash_closed",
                    EventVisibility::Internal,
                    &payload,
                )?,
                payload,
                raw_event_id: None,
                created_at: Utc::now(),
            };
            store.append_event_allocating_seq(event, None)?;
        }
        store.update_turn_state(
            turn.id,
            TurnStatus::Failed,
            effects,
            turn.native_turn_id.as_deref(),
            Utc::now(),
        )?;
        recovered.push(turn.id);
    }
    Ok(recovered)
}

/// Finds the workspace evidence belonging to one exact provider launch.
///
/// The pre-launch snapshot is mandatory. The post-launch snapshot can be
/// written by the normal wrapper exit path or by crash reconciliation after
/// the journaled process group is proven dead.
pub fn native_launch_workspace_snapshots(
    store: &SqliteStore,
    session: &UnifiedSession,
    launch: &NativeLaunchRecord,
) -> Result<NativeLaunchWorkspaceSnapshots> {
    ensure!(
        launch.session_id == session.id,
        "native launch snapshot lookup crossed canonical sessions"
    );
    let snapshots = store.list_workspace_snapshots(session.id, None)?;
    let decode = |record: &agentctl_storage::WorkspaceSnapshotRecord| -> Result<GitSnapshot> {
        ensure!(
            record.fingerprint == session.workspace_fingerprint,
            "native launch workspace snapshot fingerprint changed"
        );
        serde_json::from_value(record.snapshot.clone())
            .context("native launch workspace snapshot is invalid")
    };
    let before = snapshots
        .iter()
        .filter(|record| record.phase == "before_native" && record.created_at <= launch.started_at)
        .max_by_key(|record| (record.created_at, record.id))
        .context("native launch has no durable pre-launch workspace snapshot")?;
    let after = snapshots
        .iter()
        .filter(|record| {
            record.phase.starts_with("after_native") && record.created_at >= launch.started_at
        })
        .min_by_key(|record| (record.created_at, record.id))
        .map(decode)
        .transpose()?;
    let before = decode(before)?;
    if let Some(after) = &after {
        ensure!(
            before.root == after.root,
            "native launch workspace root changed while recovering"
        );
    }
    Ok(NativeLaunchWorkspaceSnapshots { before, after })
}

/// Persists the only safe cross-provider outcome after a crashed native turn:
/// a deterministic continuation marker. It never contains or submits a prompt.
/// The marker is appended before the launch journal is closed, so a crash in
/// between remains fail-closed and a retry observes the same event id.
#[allow(clippy::too_many_lines)]
pub fn persist_native_failover_continuation(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    launch: &NativeLaunchRecord,
) -> Result<NativeFailoverContinuation> {
    ensure!(
        launch.session_id == session.id,
        "native failover continuation crossed canonical sessions"
    );
    ensure!(
        matches!(launch.provider, ProviderKind::Claude | ProviderKind::Codex),
        "plugins cannot own a native failover continuation"
    );
    let event_id = deterministic_native_failover_event_id(session.id, launch.id);
    if let Some(existing) = store.event_by_id(event_id)? {
        ensure!(
            existing.session_id == session.id
                && existing.kind == "native_context_marker"
                && existing
                    .payload
                    .get("source_launch_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(launch.id.to_string().as_str()),
            "deterministic native failover continuation event id collision"
        );
        let side_effect_state = existing
            .payload
            .get("side_effect_state")
            .cloned()
            .map(serde_json::from_value)
            .transpose()?
            .context("native failover continuation omitted side-effect state")?;
        return Ok(NativeFailoverContinuation {
            event_id,
            seq: existing.seq,
            turn_id: existing.turn_id,
            side_effect_state,
        });
    }

    let snapshots = native_launch_workspace_snapshots(store, session, launch)?;
    let after = snapshots.after.context(
        "native failover remains blocked until a durable post-crash workspace snapshot exists",
    )?;
    let mut side_effect_state = SideEffectState::Possible;
    if after.changed_since(&snapshots.before) {
        side_effect_state = SideEffectState::Confirmed;
    }

    let events = store.list_events(session.id, 0, usize::MAX)?;
    let mut affected_turns = events
        .iter()
        .filter(|event| {
            event.created_at >= launch.started_at
                && event.origin_provider.as_ref() == Some(&launch.provider)
        })
        .filter_map(|event| event.turn_id)
        .collect::<BTreeSet<_>>();
    affected_turns.extend(
        store
            .recovery_candidates_for(session.id, &launch.provider)?
            .into_iter()
            .filter(|turn| turn.created_at >= launch.started_at)
            .map(|turn| turn.id),
    );
    let mut latest_turn = None;
    for turn_id in affected_turns {
        let turn = store
            .get_turn(turn_id)?
            .with_context(|| format!("native failover turn {turn_id} disappeared"))?;
        side_effect_state = side_effect_state.observe(turn.side_effect_state);
        if latest_turn.as_ref().is_none_or(|latest: &TurnRecord| {
            (turn.updated_at, turn.id) > (latest.updated_at, latest.id)
        }) {
            latest_turn = Some(turn);
        }
    }
    let turn_id = latest_turn.as_ref().map(|turn| turn.id);
    let last_confirmed_event_seq = events.last().map_or(0, |event| event.seq);
    let text = format!(
        "The previous native {} process ended unexpectedly after {} side effects. Continue from the current workspace and canonical history. Do not replay the original request or repeat operations already completed. Inspect the current diff, captured commands, and uncertain operations before the user submits the next instruction in the native CLI.",
        launch.provider,
        match side_effect_state {
            SideEffectState::Possible => "possible",
            SideEffectState::Confirmed => "confirmed",
            SideEffectState::None => unreachable!("crash continuation is always conservative"),
        }
    );
    let payload = guard.process_json(&serde_json::json!({
        "text": text,
        "marker_kind": "native_failover_continuation",
        "source_launch_id": launch.id,
        "source_provider": launch.provider,
        "side_effect_state": side_effect_state,
        "continuation": true,
        "replay_allowed": false,
        "user_action_required": true,
        "last_confirmed_event_seq": last_confirmed_event_seq,
        "before_diff_digest": snapshots.before.diff_digest,
        "after_diff_digest": after.diff_digest,
        "changed_paths": after.changed_paths,
    }))?;
    let event = CanonicalEvent {
        schema_version: 1,
        session_id: session.id,
        seq: 0,
        event_id,
        turn_id,
        origin_provider: Some(launch.provider.clone()),
        kind: "native_context_marker".to_owned(),
        visibility: EventVisibility::Projection,
        content_hash: canonical_content_hash(
            "native_context_marker",
            EventVisibility::Projection,
            &payload,
        )?,
        payload,
        raw_event_id: None,
        created_at: Utc::now(),
    };
    let event = store.append_event_allocating_seq(event, None)?;
    Ok(NativeFailoverContinuation {
        event_id,
        seq: event.seq,
        turn_id,
        side_effect_state,
    })
}

fn deterministic_native_failover_event_id(
    session_id: UnifiedSessionId,
    launch_id: Uuid,
) -> EventId {
    let material = format!("native-failover-continuation\0{session_id}\0{launch_id}");
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EventId(Uuid::from_bytes(bytes))
}

fn deterministic_native_crash_turn_event_id(
    session_id: UnifiedSessionId,
    launch_id: Uuid,
    turn_id: TurnId,
) -> EventId {
    let material = format!("native-turn-crash-closed\0{session_id}\0{launch_id}\0{turn_id}");
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EventId(Uuid::from_bytes(bytes))
}

/// Holds every writer lease needed to keep a local state mutation from racing
/// a provider-owned foreground CLI. The journal check runs only after the OS
/// lease is acquired, closing the check/start time-of-check window.
#[derive(Debug)]
#[must_use]
pub struct WorkspaceMutationGuard {
    _leases: Vec<WorkspaceLease>,
    session_ids: Vec<UnifiedSessionId>,
}

impl WorkspaceMutationGuard {
    fn session_ids(&self) -> &[UnifiedSessionId] {
        &self.session_ids
    }
}

pub fn guard_session_mutation(
    paths: &AgentctlPaths,
    store: &SqliteStore,
    session: &UnifiedSession,
    operation: &str,
) -> Result<WorkspaceMutationGuard> {
    let identity = WorkspaceIdentity::discover(&session.workspace_path)
        .with_context(|| format!("{operation} refused: failed to identify the session worktree"))?;
    let lease =
        WorkspaceLease::acquire_for_identity(&paths.locks, &identity, session.id, TurnId::new())
            .with_context(|| {
                format!(
                    "{operation} refused: another agentctl process owns worktree {}",
                    identity.execution_root().display()
                )
            })?;
    ensure_session_mutation_idle(store, session, &identity, operation)?;
    Ok(WorkspaceMutationGuard {
        _leases: vec![lease],
        session_ids: vec![session.id],
    })
}

/// Checks both the stable worktree identity and canonical session. The second
/// query catches older journal entries whose recorded lease key no longer
/// matches after a workspace repair.
pub fn ensure_session_mutation_idle(
    store: &SqliteStore,
    session: &UnifiedSession,
    identity: &WorkspaceIdentity,
    operation: &str,
) -> Result<()> {
    ensure_session_mutation_idle_except(store, session, identity, operation, None)
}

fn ensure_session_mutation_idle_except(
    store: &SqliteStore,
    session: &UnifiedSession,
    identity: &WorkspaceIdentity,
    operation: &str,
    ignored_launch: Option<Uuid>,
) -> Result<()> {
    let mut open = store.open_native_launch_for_workspace(&identity.lease_key)?;
    if open.is_none() {
        open = store.open_native_launches(session.id)?.into_iter().next();
    }
    if let Some(launch) = open {
        if ignored_launch == Some(launch.id) {
            return Ok(());
        }
        bail!(
            "{operation} refused: native {} launch {} for session {} is {:?}; exit and let capture finish, or reconcile the uncertain launch, before mutating canonical state",
            launch.provider,
            launch.id,
            launch.session_id,
            launch.state
        );
    }
    Ok(())
}

fn guard_all_session_mutations(
    paths: &AgentctlPaths,
    store: &SqliteStore,
    operation: &str,
    ignored_launch: Option<Uuid>,
) -> Result<WorkspaceMutationGuard> {
    let sessions = store.list_sessions()?;
    let mut mapped = Vec::with_capacity(sessions.len());
    let mut worktrees = BTreeMap::new();
    for session in sessions {
        let identity = WorkspaceIdentity::discover(&session.workspace_path).with_context(|| {
            format!(
                "{operation} refused: failed to identify worktree for session {}",
                session.id
            )
        })?;
        worktrees
            .entry(identity.lease_key.clone())
            .or_insert_with(|| (identity.clone(), session.id));
        mapped.push((session, identity));
    }

    // BTreeMap ordering makes concurrent global maintenance acquire multiple
    // worktree locks in the same order.
    let mut leases = Vec::with_capacity(worktrees.len());
    for (_, (identity, session_id)) in worktrees {
        leases.push(
            WorkspaceLease::acquire_for_identity(
                &paths.locks,
                &identity,
                session_id,
                TurnId::new(),
            )
            .with_context(|| {
                format!(
                    "{operation} refused: another agentctl process owns worktree {}",
                    identity.execution_root().display()
                )
            })?,
        );
    }
    for (session, identity) in &mapped {
        ensure_session_mutation_idle_except(store, session, identity, operation, ignored_launch)?;
    }
    let session_ids = mapped.iter().map(|(session, _)| session.id).collect();
    Ok(WorkspaceMutationGuard {
        _leases: leases,
        session_ids,
    })
}

/// Resolves an explicit UUID/name/path, or the most recently updated session for cwd.
pub fn resolve_session(
    store: &SqliteStore,
    selector: Option<&str>,
    cwd: &Path,
) -> Result<UnifiedSession> {
    let sessions = store
        .list_sessions()
        .context("failed to list canonical sessions")?;
    if let Some(selector) = selector.map(str::trim).filter(|value| !value.is_empty()) {
        if let Ok(id) = UnifiedSessionId::from_str(selector) {
            return store
                .get_session(id)?
                .with_context(|| format!("session {id} does not exist"));
        }

        let named: Vec<_> = sessions
            .iter()
            .filter(|session| session.name == selector)
            .cloned()
            .collect();
        match named.as_slice() {
            [session] => return Ok(session.clone()),
            [] => {}
            _ => {
                let ids = named
                    .iter()
                    .map(|session| session.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("session name {selector:?} is ambiguous; matching IDs: {ids}");
            }
        }

        let selected_path = Path::new(selector);
        if selected_path.exists() {
            return resolve_session_for_path(&sessions, selected_path)
                .with_context(|| format!("no session belongs to {}", selected_path.display()));
        }
        bail!("session {selector:?} does not exist");
    }

    resolve_session_for_path(&sessions, cwd)
        .with_context(|| format!("no session belongs to current workspace {}", cwd.display()))
}

fn resolve_session_for_path(sessions: &[UnifiedSession], path: &Path) -> Option<UnifiedSession> {
    let requested = normalize_existing_path(path);
    sessions
        .iter()
        .filter_map(|session| {
            let workspace = normalize_existing_path(&session.workspace_path);
            (requested == workspace || requested.starts_with(&workspace))
                .then_some((workspace.components().count(), session))
        })
        .max_by(|(left_depth, left), (right_depth, right)| {
            left_depth
                .cmp(right_depth)
                .then_with(|| left.updated_at.cmp(&right.updated_at))
        })
        .map(|(_, session)| session.clone())
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionListEntry {
    pub session: UnifiedSession,
    pub latest_seq: u64,
    pub provider_count: usize,
}

pub fn list_sessions(store: &SqliteStore) -> Result<Vec<SessionListEntry>> {
    store
        .list_sessions()?
        .into_iter()
        .map(|session| {
            let latest_seq = latest_seq(store, session.id)?;
            let provider_count = store.list_provider_sessions(session.id)?.len();
            Ok(SessionListEntry {
                session,
                latest_seq,
                provider_count,
            })
        })
        .collect()
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderStatusView {
    pub session: ProviderSessionRecord,
    pub sync_lag: u64,
    pub health: Option<ProviderHealth>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceStatusView {
    pub exists: bool,
    pub identity: Option<WorkspaceIdentity>,
    pub fingerprint_changed: Option<bool>,
    pub git: Option<GitSnapshot>,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionStatusView {
    pub session: UnifiedSession,
    pub latest_seq: u64,
    pub providers: Vec<ProviderStatusView>,
    pub workspace: WorkspaceStatusView,
}

pub fn session_status(store: &SqliteStore, session: UnifiedSession) -> Result<SessionStatusView> {
    let latest_seq = latest_seq(store, session.id)?;
    let providers = store
        .list_provider_sessions(session.id)?
        .into_iter()
        .map(|provider| {
            let health = store.latest_health(&provider.provider)?;
            Ok(ProviderStatusView {
                sync_lag: latest_seq.saturating_sub(provider.last_synced_seq),
                session: provider,
                health,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let workspace = inspect_workspace(&session.workspace_path, &session.workspace_fingerprint);
    Ok(SessionStatusView {
        session,
        latest_seq,
        providers,
        workspace,
    })
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageAggregate {
    pub snapshots: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cost_usd: f64,
}

impl UsageAggregate {
    fn observe(&mut self, usage: &UsageSnapshot) {
        self.snapshots = self.snapshots.saturating_add(1);
        self.input_tokens = self
            .input_tokens
            .saturating_add(usage.input_tokens.unwrap_or(0));
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.output_tokens.unwrap_or(0));
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens.unwrap_or(0));
        self.cost_usd += usage.cost_usd.unwrap_or(0.0);
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CanonicalMetrics {
    pub event_count: u64,
    pub turn_count: usize,
    pub raw_event_links: u64,
    pub by_kind: BTreeMap<String, u64>,
    pub by_visibility: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProviderMetrics {
    pub provider: ProviderKind,
    pub native_session: Option<ProviderSessionRecord>,
    pub event_count: u64,
    pub assistant_finals: u64,
    pub errors: u64,
    pub usage: UsageAggregate,
    pub sync_lag: u64,
    pub health: Option<ProviderHealth>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncMetrics {
    pub latest_seq: u64,
    pub providers_behind: usize,
    pub total_lag: u64,
    pub max_lag: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionMetrics {
    pub session: UnifiedSession,
    pub canonical: CanonicalMetrics,
    pub providers: Vec<ProviderMetrics>,
    pub usage: UsageAggregate,
    pub sync: SyncMetrics,
}

#[allow(clippy::too_many_lines)]
pub fn session_metrics(store: &SqliteStore, session: UnifiedSession) -> Result<SessionMetrics> {
    let events = store.list_events(session.id, 0, usize::MAX)?;
    let latest_seq = events.last().map_or(0, |event| event.seq);
    let provider_sessions = store.list_provider_sessions(session.id)?;
    let mut turns = BTreeSet::new();
    let mut by_kind = BTreeMap::new();
    let mut by_visibility = BTreeMap::new();
    let mut raw_event_links = 0_u64;
    let mut usage = UsageAggregate::default();
    let mut providers = BTreeMap::<ProviderKind, ProviderMetrics>::new();

    for record in provider_sessions {
        let health = store.latest_health(&record.provider)?;
        let sync_lag = latest_seq.saturating_sub(record.last_synced_seq);
        providers.insert(
            record.provider.clone(),
            ProviderMetrics {
                provider: record.provider.clone(),
                native_session: Some(record),
                event_count: 0,
                assistant_finals: 0,
                errors: 0,
                usage: UsageAggregate::default(),
                sync_lag,
                health,
            },
        );
    }

    for event in &events {
        if let Some(turn_id) = event.turn_id {
            turns.insert(turn_id);
        }
        *by_kind.entry(event.kind.clone()).or_default() += 1;
        let visibility = match event.visibility {
            EventVisibility::User => "user",
            EventVisibility::Projection => "projection",
            EventVisibility::Internal => "internal",
        };
        *by_visibility.entry(visibility.to_owned()).or_default() += 1;
        raw_event_links = raw_event_links.saturating_add(u64::from(event.raw_event_id.is_some()));

        let provider_metrics = event.origin_provider.as_ref().map(|provider| {
            providers
                .entry(provider.clone())
                .or_insert_with(|| ProviderMetrics {
                    provider: provider.clone(),
                    native_session: None,
                    event_count: 0,
                    assistant_finals: 0,
                    errors: 0,
                    usage: UsageAggregate::default(),
                    sync_lag: latest_seq,
                    health: None,
                })
        });
        if let Some(metrics) = provider_metrics {
            metrics.event_count = metrics.event_count.saturating_add(1);
            metrics.assistant_finals = metrics
                .assistant_finals
                .saturating_add(u64::from(event.kind == "assistant_final"));
            metrics.errors = metrics
                .errors
                .saturating_add(u64::from(event.kind == "error"));
        }

        if event.kind == "usage_updated"
            && let Ok(AgentEvent::UsageUpdated { usage: snapshot }) =
                serde_json::from_value::<AgentEvent>(event.payload.clone())
        {
            usage.observe(&snapshot);
            if let Some(provider) = &event.origin_provider
                && let Some(metrics) = providers.get_mut(provider)
            {
                metrics.usage.observe(&snapshot);
            }
        }
    }

    let provider_values = providers.into_values().collect::<Vec<_>>();
    let total_lag = provider_values
        .iter()
        .fold(0_u64, |sum, provider| sum.saturating_add(provider.sync_lag));
    let max_lag = provider_values
        .iter()
        .map(|provider| provider.sync_lag)
        .max()
        .unwrap_or(0);
    let providers_behind = provider_values
        .iter()
        .filter(|provider| provider.sync_lag > 0)
        .count();

    Ok(SessionMetrics {
        session,
        canonical: CanonicalMetrics {
            event_count: u64::try_from(events.len()).unwrap_or(u64::MAX),
            turn_count: turns.len(),
            raw_event_links,
            by_kind,
            by_visibility,
        },
        providers: provider_values,
        usage,
        sync: SyncMetrics {
            latest_seq,
            providers_behind,
            total_lag,
            max_lag,
        },
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeAttachmentReport {
    pub session_id: UnifiedSessionId,
    pub provider: ProviderKind,
    pub native_session_id: String,
    pub attached: bool,
    pub activated: bool,
    pub validation: &'static str,
    pub canonical_history_imported: bool,
    pub canonical_events_before_attach: u64,
    pub warning: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeImportReport {
    pub session_id: UnifiedSessionId,
    pub provider: ProviderKind,
    pub native_session_id: String,
    pub imported_turns: usize,
    pub imported_events: usize,
    pub skipped_existing_events: usize,
    pub activated: bool,
    pub canonical_history_imported: bool,
    pub validation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Clone, Debug)]
struct NativeCaptureExpectation {
    content_digest: String,
    kind: &'static str,
    turn_id: TurnId,
}

/// Captures the new public events of a Codex transcript after the user exits
/// the native CLI. Unlike [`persist_native_import`], this operation is designed
/// for a transcript that grows over time: previously captured native turn/item
/// keys are skipped and only the append-only delta is added to the canonical
/// log.
///
/// The native session must already be linked to the unified session and fully
/// synchronized through the canonical history that preceded the native run.
/// This prevents advancing the projection cursor over unrelated events that
/// the native session has never seen.
#[allow(clippy::too_many_lines)]
pub fn persist_native_capture(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    native: &NativeSession,
    transcript: &NativeTranscript,
) -> Result<NativeImportReport> {
    ensure!(
        native.provider == ProviderKind::Codex
            && transcript.provider == native.provider
            && transcript.native_session_id == native.native_session_id,
        "native transcript identity does not match the linked Codex session"
    );
    let provider_record = store
        .provider_session(session.id, &native.provider)?
        .context("Codex native session is not linked; open or attach it before capture")?;
    ensure!(
        provider_record.native_session_id == native.native_session_id,
        "Codex is linked to native session {}, not {}",
        provider_record.native_session_id,
        native.native_session_id
    );
    ensure!(
        store.pending_sync_intents(provider_record.id)?.is_empty(),
        "Codex projection has an uncertain send; repair it before capturing native history"
    );

    let canonical_workspace = WorkspaceIdentity::discover(&session.workspace_path)
        .context("canonical session workspace is unavailable")?;
    let native_workspace = WorkspaceIdentity::discover(&transcript.workspace_cwd)
        .context("native transcript workspace is unavailable")?;
    ensure!(
        native_workspace.lease_key == canonical_workspace.lease_key,
        "native transcript workspace {} does not match canonical workspace {}",
        native_workspace.execution_root().display(),
        canonical_workspace.execution_root().display()
    );
    ensure!(
        transcript.turns.iter().all(|turn| !matches!(
            turn.status,
            TurnStatus::Pending
                | TurnStatus::Running
                | TurnStatus::WaitingOnApproval
                | TurnStatus::Uncertain
        )),
        "refusing to capture a native transcript with a non-terminal turn; exit the native Codex session cleanly and retry"
    );

    let existing = store.list_events(session.id, 0, usize::MAX)?;
    let session_digest = native_import_digest(&native.native_session_id);
    ensure!(
        provider_record.last_synced_seq <= existing.last().map_or(0, |event| event.seq),
        "Codex projection cursor is ahead of canonical history"
    );
    for event in existing
        .iter()
        .filter(|event| event.seq > provider_record.last_synced_seq)
    {
        let harmless_empty_attachment = event.kind == "native_session_attached"
            && event
                .payload
                .get("native_session_id")
                .and_then(serde_json::Value::as_str)
                == Some(native.native_session_id.as_str())
            && event
                .payload
                .get("canonical_events_before_attach")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|seq| seq <= provider_record.last_synced_seq);
        ensure!(
            harmless_empty_attachment
                || event
                    .payload
                    .pointer("/native_capture/session_digest")
                    .and_then(serde_json::Value::as_str)
                    == Some(session_digest.as_str()),
            "Codex has canonical sync lag at event #{}; restore that history before capturing a native run",
            event.seq
        );
    }

    let expectations = preflight_native_capture(store, guard, session, native, transcript)?;
    let mut captured_keys = BTreeSet::new();
    for event in &existing {
        let capture_key = event
            .payload
            .pointer("/native_capture/key_digest")
            .and_then(serde_json::Value::as_str);
        let imported_key = event
            .payload
            .pointer("/native_import/key_digest")
            .and_then(serde_json::Value::as_str);
        let Some(key_digest) = capture_key.or(imported_key) else {
            continue;
        };
        ensure!(
            captured_keys.insert(key_digest.to_owned()),
            "canonical history contains duplicate native event key {key_digest}"
        );
        if capture_key.is_some() {
            let expectation = expectations.get(key_digest).with_context(|| {
                format!(
                    "thread/read omitted previously captured native event key {key_digest}; refusing a non-monotonic transcript"
                )
            })?;
            ensure!(
                event
                    .payload
                    .pointer("/native_capture/content_digest")
                    .and_then(serde_json::Value::as_str)
                    == Some(expectation.content_digest.as_str())
                    && event.kind == expectation.kind
                    && event.turn_id == Some(expectation.turn_id),
                "native transcript changed content under an already captured turn/item ID"
            );
        }
    }

    let previously_captured = captured_keys.clone();
    let mut imported_events = 0_usize;
    let mut skipped_existing_events = 0_usize;
    let mut imported_turns = 0_usize;
    let now = Utc::now();

    for turn in &transcript.turns {
        let turn_id = deterministic_import_turn_id(
            session.id,
            &native.provider,
            &native.native_session_id,
            &turn.native_turn_id,
        );
        let side_effect_state = native_turn_side_effect_state(turn);
        let turn_existed = store.get_turn(turn_id)?.is_some();
        if !turn_existed {
            store.create_turn(&TurnRecord {
                id: turn_id,
                session_id: session.id,
                provider: Some(native.provider.clone()),
                prompt_seq: store.next_seq(session.id)?,
                status: turn.status,
                side_effect_state,
                native_turn_id: Some(turn.native_turn_id.clone()),
                continuation: false,
                started_at: Some(now),
                completed_at: Some(now),
                created_at: now,
                updated_at: now,
            })?;
        }

        let events_before_turn = imported_events;
        for item in &turn.items {
            let native_item_id = native_transcript_item_id(item);
            let base = format!("{}:{native_item_id}", turn.native_turn_id);
            let item_digest = native_capture_content_digest(item)?;
            match item {
                NativeTranscriptItem::UserPrompt {
                    text,
                    attachments,
                    raw,
                    ..
                } => {
                    imported_events += append_native_capture_event(
                        store,
                        guard,
                        session.id,
                        turn_id,
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        &item_digest,
                        "user_prompt",
                        EventVisibility::User,
                        &serde_json::json!({"text": text, "attachments": attachments}),
                        Some(("thread/read:userMessage", raw)),
                        &mut captured_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::AssistantMessage {
                    text,
                    final_answer,
                    raw,
                    ..
                } => {
                    imported_events += append_native_capture_event(
                        store,
                        guard,
                        session.id,
                        turn_id,
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        &item_digest,
                        if *final_answer {
                            "assistant_final"
                        } else {
                            "assistant_text_delta"
                        },
                        if *final_answer {
                            EventVisibility::User
                        } else {
                            EventVisibility::Internal
                        },
                        &serde_json::json!({"text": text}),
                        Some(("thread/read:agentMessage", raw)),
                        &mut captured_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::Plan { text, raw, .. } => {
                    imported_events += append_native_capture_event(
                        store,
                        guard,
                        session.id,
                        turn_id,
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        &item_digest,
                        "plan_updated",
                        EventVisibility::User,
                        &serde_json::json!({"steps": [{"text": text, "status": "completed"}]}),
                        Some(("thread/read:plan", raw)),
                        &mut captured_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::Command {
                    command,
                    cwd,
                    exit_code,
                    status,
                    raw,
                    ..
                } => {
                    if *status == NativeEffectStatus::Declined {
                        imported_events += append_native_capture_event(
                            store,
                            guard,
                            session.id,
                            turn_id,
                            &native.provider,
                            &native.native_session_id,
                            &format!("{base}:declined"),
                            &item_digest,
                            "command_declined",
                            EventVisibility::User,
                            &serde_json::json!({
                                "command": command,
                                "cwd": cwd.as_deref().unwrap_or("."),
                                "status": status,
                            }),
                            Some(("thread/read:commandExecution", raw)),
                            &mut captured_keys,
                            &mut skipped_existing_events,
                        )?;
                    } else {
                        let command_id = native_import_digest(&format!(
                            "{}\0{native_item_id}",
                            turn.native_turn_id
                        ));
                        imported_events += append_native_capture_event(
                            store,
                            guard,
                            session.id,
                            turn_id,
                            &native.provider,
                            &native.native_session_id,
                            &format!("{base}:started"),
                            &item_digest,
                            "command_started",
                            EventVisibility::User,
                            &serde_json::json!({
                                "id": command_id,
                                "command": command,
                                "cwd": cwd.as_deref().unwrap_or("."),
                            }),
                            Some(("thread/read:commandExecution", raw)),
                            &mut captured_keys,
                            &mut skipped_existing_events,
                        )?;
                        imported_events += append_native_capture_event(
                            store,
                            guard,
                            session.id,
                            turn_id,
                            &native.provider,
                            &native.native_session_id,
                            &format!("{base}:completed"),
                            &item_digest,
                            "command_completed",
                            EventVisibility::User,
                            &serde_json::json!({
                                "id": command_id,
                                "exit_code": exit_code,
                                "output_digest": null,
                                "status": status,
                            }),
                            None,
                            &mut captured_keys,
                            &mut skipped_existing_events,
                        )?;
                    }
                }
                NativeTranscriptItem::FilesChanged {
                    paths, status, raw, ..
                } => {
                    let (key, kind, payload) = match status {
                        NativeEffectStatus::Completed => (
                            base,
                            "files_changed",
                            serde_json::json!({
                                "changes": paths.iter().map(|path| serde_json::json!({
                                    "path": path,
                                    "kind": "modified",
                                    "digest": null,
                                })).collect::<Vec<_>>()
                            }),
                        ),
                        NativeEffectStatus::Failed => (
                            format!("{base}:failed"),
                            "file_change_failed",
                            serde_json::json!({"paths": paths, "status": status}),
                        ),
                        NativeEffectStatus::Declined => (
                            format!("{base}:declined"),
                            "file_change_declined",
                            serde_json::json!({"paths": paths, "status": status}),
                        ),
                    };
                    imported_events += append_native_capture_event(
                        store,
                        guard,
                        session.id,
                        turn_id,
                        &native.provider,
                        &native.native_session_id,
                        &key,
                        &item_digest,
                        kind,
                        EventVisibility::User,
                        &payload,
                        Some(("thread/read:fileChange", raw)),
                        &mut captured_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::ToolCall {
                    name,
                    input_summary,
                    status,
                    output_digest,
                    artifacts,
                    raw,
                    ..
                } => {
                    let tool_id =
                        native_import_digest(&format!("{}\0{native_item_id}", turn.native_turn_id));
                    if *status != NativeEffectStatus::Declined {
                        imported_events += append_native_capture_event(
                            store,
                            guard,
                            session.id,
                            turn_id,
                            &native.provider,
                            &native.native_session_id,
                            &format!("{base}:started"),
                            &item_digest,
                            "tool_started",
                            EventVisibility::User,
                            &serde_json::json!({
                                "id": tool_id,
                                "name": name,
                                "input": input_summary,
                            }),
                            Some(("thread/read:toolCall", raw)),
                            &mut captured_keys,
                            &mut skipped_existing_events,
                        )?;
                    }
                    let completion_key = if *status == NativeEffectStatus::Declined {
                        format!("{base}:declined")
                    } else {
                        format!("{base}:completed")
                    };
                    imported_events += append_native_capture_event(
                        store,
                        guard,
                        session.id,
                        turn_id,
                        &native.provider,
                        &native.native_session_id,
                        &completion_key,
                        &item_digest,
                        "tool_completed",
                        EventVisibility::User,
                        &serde_json::json!({
                            "id": tool_id,
                            "name": name,
                            "status": status,
                            "output": {
                                "summary": format!("{name}: {status:?}"),
                                "digest": output_digest,
                                "artifacts": artifacts,
                            },
                        }),
                        (*status == NativeEffectStatus::Declined)
                            .then_some(("thread/read:toolCall", raw)),
                        &mut captured_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::ContextMarker {
                    marker_kind,
                    summary,
                    content_digest,
                    raw,
                    ..
                } => {
                    imported_events += append_native_capture_event(
                        store,
                        guard,
                        session.id,
                        turn_id,
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        &item_digest,
                        "native_context_marker",
                        EventVisibility::User,
                        &serde_json::json!({
                            "kind": marker_kind,
                            "text": summary,
                            "content_digest": content_digest,
                        }),
                        Some(("thread/read:contextMarker", raw)),
                        &mut captured_keys,
                        &mut skipped_existing_events,
                    )?;
                }
            }
        }
        let completion_digest = native_import_digest(&serde_json::to_string(&serde_json::json!({
            "native_turn_id": turn.native_turn_id,
            "status": turn.status,
        }))?);
        imported_events += append_native_capture_event(
            store,
            guard,
            session.id,
            turn_id,
            &native.provider,
            &native.native_session_id,
            &format!("{}:completed", turn.native_turn_id),
            &completion_digest,
            "turn_completed",
            EventVisibility::User,
            &serde_json::json!({"status": turn.status}),
            None,
            &mut captured_keys,
            &mut skipped_existing_events,
        )?;
        store.update_turn_state(
            turn_id,
            turn.status,
            side_effect_state,
            Some(&turn.native_turn_id),
            now,
        )?;
        if imported_events > events_before_turn {
            imported_turns = imported_turns.saturating_add(1);
        }
    }

    let latest_captured_seq = store.next_seq(session.id)?.saturating_sub(1);
    store.advance_provider_cursor(provider_record.id, latest_captured_seq)?;
    let activated = store
        .get_session(session.id)?
        .is_some_and(|stored| stored.active_provider.as_ref() == Some(&native.provider));
    Ok(NativeImportReport {
        session_id: session.id,
        provider: native.provider.clone(),
        native_session_id: native.native_session_id.clone(),
        imported_turns,
        imported_events,
        skipped_existing_events,
        activated,
        canonical_history_imported: true,
        validation: "official_thread_read_incremental",
        warning: (!previously_captured.is_empty() && imported_events == 0)
            .then(|| "native transcript was already fully captured".to_owned()),
    })
}

/// Imports a provider transcript that was obtained through an official native
/// history API. The operation is idempotent at native turn/item granularity so
/// an interrupted import can be safely resumed.
#[allow(clippy::too_many_lines)]
pub fn persist_native_import(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    native: &NativeSession,
    transcript: &NativeTranscript,
    activate: bool,
) -> Result<NativeImportReport> {
    ensure!(
        native.provider == transcript.provider
            && native.native_session_id == transcript.native_session_id,
        "native transcript identity does not match the attached provider session"
    );
    ensure!(
        native.provider == ProviderKind::Codex,
        "{} does not expose a supported public transcript import API; use attach instead",
        native.provider
    );
    let canonical_workspace = WorkspaceIdentity::discover(&session.workspace_path)
        .context("canonical session workspace is unavailable")?;
    let native_workspace = WorkspaceIdentity::discover(&transcript.workspace_cwd)
        .context("native transcript workspace is unavailable")?;
    ensure!(
        native_workspace.lease_key == canonical_workspace.lease_key,
        "native transcript workspace {} does not match canonical workspace {}",
        native_workspace.execution_root().display(),
        canonical_workspace.execution_root().display()
    );
    ensure!(
        transcript.turns.iter().all(|turn| !matches!(
            turn.status,
            TurnStatus::Pending
                | TurnStatus::Running
                | TurnStatus::WaitingOnApproval
                | TurnStatus::Uncertain
        )),
        "refusing to import a native transcript with a non-terminal turn; attach and resume it instead"
    );

    let existing = store.list_events(session.id, 0, usize::MAX)?;
    let import_session_digest = native_import_digest(&native.native_session_id);
    for event in &existing {
        let same_import = event
            .payload
            .pointer("/native_import/session_digest")
            .and_then(serde_json::Value::as_str)
            == Some(import_session_digest.as_str());
        let same_attachment = event.kind == "native_session_attached"
            && event
                .payload
                .get("native_session_id")
                .and_then(serde_json::Value::as_str)
                == Some(native.native_session_id.as_str());
        ensure!(
            same_import || same_attachment,
            "canonical session already contains unrelated history; import into a new empty session to preserve ordering"
        );
    }
    if let Some(record) = store.provider_session(session.id, &native.provider)? {
        ensure!(
            record.native_session_id == native.native_session_id,
            "{} already projects to another native session",
            native.provider
        );
    }

    let transcript_digest = native_transcript_digest(transcript)?;
    let import_start_key = format!("import:started:{transcript_digest}");
    let import_start_key_digest =
        native_import_digest(&format!("{}\0{import_start_key}", native.native_session_id));
    let has_partial_import = existing.iter().any(|event| {
        event
            .payload
            .pointer("/native_import/key_digest")
            .and_then(serde_json::Value::as_str)
            .is_some()
    });
    let import_committed = existing
        .iter()
        .any(|event| event.kind == "native_session_imported");
    if has_partial_import {
        ensure!(
            existing.iter().any(|event| {
                event.kind == "native_session_import_started"
                    && event
                        .payload
                        .pointer("/native_import/key_digest")
                        .and_then(serde_json::Value::as_str)
                        == Some(import_start_key_digest.as_str())
            }),
            "{}",
            if import_committed {
                "native transcript changed after its canonical import was committed"
            } else {
                "native transcript changed while a prior canonical import was incomplete"
            }
        );
    }
    let mut expected_import_keys =
        preflight_native_import(store, guard, session, native, transcript)?;
    expected_import_keys.insert(import_start_key_digest);

    let mut imported_keys = existing
        .iter()
        .filter_map(|event| {
            event
                .payload
                .pointer("/native_import/key_digest")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect::<BTreeSet<_>>();
    if import_committed {
        ensure!(
            imported_keys == expected_import_keys,
            "native transcript changed after its canonical import was committed"
        );
    }
    let mut imported_events = 0_usize;
    let mut skipped_existing_events = 0_usize;
    let now = Utc::now();

    imported_events += append_native_import_event(
        store,
        guard,
        session.id,
        None,
        &native.provider,
        &native.native_session_id,
        &import_start_key,
        "native_session_import_started",
        EventVisibility::Internal,
        &serde_json::json!({
            "provider": native.provider,
            "turn_count": transcript.turns.len(),
            "validation": "official_thread_read",
        }),
        None,
        &mut imported_keys,
        &mut skipped_existing_events,
    )?;

    for turn in &transcript.turns {
        let turn_id = deterministic_import_turn_id(
            session.id,
            &native.provider,
            &native.native_session_id,
            &turn.native_turn_id,
        );
        let side_effect_state = native_turn_side_effect_state(turn);
        if let Some(existing_turn) = store.get_turn(turn_id)? {
            ensure!(
                existing_turn.native_turn_id.as_deref() == Some(turn.native_turn_id.as_str())
                    && existing_turn.provider.as_ref() == Some(&native.provider),
                "deterministic native import turn collision"
            );
        } else {
            store.create_turn(&TurnRecord {
                id: turn_id,
                session_id: session.id,
                provider: Some(native.provider.clone()),
                prompt_seq: store.next_seq(session.id)?,
                status: turn.status,
                side_effect_state,
                native_turn_id: Some(turn.native_turn_id.clone()),
                continuation: false,
                started_at: Some(now),
                completed_at: Some(now),
                created_at: now,
                updated_at: now,
            })?;
        }

        for item in &turn.items {
            let native_item_id = match item {
                NativeTranscriptItem::UserPrompt { native_item_id, .. }
                | NativeTranscriptItem::AssistantMessage { native_item_id, .. }
                | NativeTranscriptItem::Plan { native_item_id, .. }
                | NativeTranscriptItem::Command { native_item_id, .. }
                | NativeTranscriptItem::FilesChanged { native_item_id, .. }
                | NativeTranscriptItem::ToolCall { native_item_id, .. }
                | NativeTranscriptItem::ContextMarker { native_item_id, .. } => native_item_id,
            };
            let base = format!("{}:{native_item_id}", turn.native_turn_id);
            match item {
                NativeTranscriptItem::UserPrompt {
                    text,
                    attachments,
                    raw,
                    ..
                } => {
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        "user_prompt",
                        EventVisibility::User,
                        &serde_json::json!({"text": text, "attachments": attachments}),
                        Some(("thread/read:userMessage", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::AssistantMessage {
                    text,
                    final_answer,
                    raw,
                    ..
                } => {
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        if *final_answer {
                            "assistant_final"
                        } else {
                            "assistant_text_delta"
                        },
                        if *final_answer {
                            EventVisibility::User
                        } else {
                            EventVisibility::Internal
                        },
                        &serde_json::json!({"text": text}),
                        Some(("thread/read:agentMessage", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::Plan { text, raw, .. } => {
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        "plan_updated",
                        EventVisibility::User,
                        &serde_json::json!({"steps": [{"text": text, "status": "completed"}]}),
                        Some(("thread/read:plan", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::Command {
                    command,
                    cwd,
                    exit_code,
                    status,
                    raw,
                    ..
                } => {
                    if *status == NativeEffectStatus::Declined {
                        imported_events += append_native_import_event(
                            store,
                            guard,
                            session.id,
                            Some(turn_id),
                            &native.provider,
                            &native.native_session_id,
                            &format!("{base}:declined"),
                            "command_declined",
                            EventVisibility::User,
                            &serde_json::json!({
                                "command": command,
                                "cwd": cwd.as_deref().unwrap_or("."),
                                "status": status,
                            }),
                            Some(("thread/read:commandExecution", raw)),
                            &mut imported_keys,
                            &mut skipped_existing_events,
                        )?;
                        continue;
                    }
                    let command_id =
                        native_import_digest(&format!("{}\0{native_item_id}", turn.native_turn_id));
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &format!("{base}:started"),
                        "command_started",
                        EventVisibility::User,
                        &serde_json::json!({
                            "id": command_id,
                            "command": command,
                            "cwd": cwd.as_deref().unwrap_or("."),
                        }),
                        Some(("thread/read:commandExecution", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &format!("{base}:completed"),
                        "command_completed",
                        EventVisibility::User,
                        &serde_json::json!({
                            "id": command_id,
                            "exit_code": exit_code,
                            "output_digest": null,
                            "status": status,
                        }),
                        None,
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::FilesChanged {
                    paths, status, raw, ..
                } => {
                    let (key, kind, payload) = match status {
                        NativeEffectStatus::Completed => (
                            base.clone(),
                            "files_changed",
                            serde_json::json!({
                                "changes": paths.iter().map(|path| serde_json::json!({
                                    "path": path,
                                    "kind": "modified",
                                    "digest": null,
                                })).collect::<Vec<_>>()
                            }),
                        ),
                        NativeEffectStatus::Failed => (
                            format!("{base}:failed"),
                            "file_change_failed",
                            serde_json::json!({"paths": paths, "status": status}),
                        ),
                        NativeEffectStatus::Declined => (
                            format!("{base}:declined"),
                            "file_change_declined",
                            serde_json::json!({"paths": paths, "status": status}),
                        ),
                    };
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &key,
                        kind,
                        EventVisibility::User,
                        &payload,
                        Some(("thread/read:fileChange", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::ToolCall {
                    name,
                    input_summary,
                    status,
                    output_digest,
                    artifacts,
                    raw,
                    ..
                } => {
                    let tool_id =
                        native_import_digest(&format!("{}\0{native_item_id}", turn.native_turn_id));
                    if *status != NativeEffectStatus::Declined {
                        imported_events += append_native_import_event(
                            store,
                            guard,
                            session.id,
                            Some(turn_id),
                            &native.provider,
                            &native.native_session_id,
                            &format!("{base}:started"),
                            "tool_started",
                            EventVisibility::User,
                            &serde_json::json!({
                                "id": tool_id,
                                "name": name,
                                "input": input_summary,
                            }),
                            Some(("thread/read:toolCall", raw)),
                            &mut imported_keys,
                            &mut skipped_existing_events,
                        )?;
                    }
                    let completion_key = if *status == NativeEffectStatus::Declined {
                        format!("{base}:declined")
                    } else {
                        format!("{base}:completed")
                    };
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &completion_key,
                        "tool_completed",
                        EventVisibility::User,
                        &serde_json::json!({
                            "id": tool_id,
                            "name": name,
                            "status": status,
                            "output": {
                                "summary": format!("{name}: {status:?}"),
                                "digest": output_digest,
                                "artifacts": artifacts,
                            },
                        }),
                        (*status == NativeEffectStatus::Declined)
                            .then_some(("thread/read:toolCall", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
                NativeTranscriptItem::ContextMarker {
                    marker_kind,
                    summary,
                    content_digest,
                    raw,
                    ..
                } => {
                    imported_events += append_native_import_event(
                        store,
                        guard,
                        session.id,
                        Some(turn_id),
                        &native.provider,
                        &native.native_session_id,
                        &base,
                        "native_context_marker",
                        EventVisibility::User,
                        &serde_json::json!({
                            "kind": marker_kind,
                            "text": summary,
                            "content_digest": content_digest,
                        }),
                        Some(("thread/read:contextMarker", raw)),
                        &mut imported_keys,
                        &mut skipped_existing_events,
                    )?;
                }
            }
        }
        imported_events += append_native_import_event(
            store,
            guard,
            session.id,
            Some(turn_id),
            &native.provider,
            &native.native_session_id,
            &format!("{}:completed", turn.native_turn_id),
            "turn_completed",
            EventVisibility::User,
            &serde_json::json!({"status": turn.status}),
            None,
            &mut imported_keys,
            &mut skipped_existing_events,
        )?;
    }

    // The completion marker is the commit point observed by Runtime startup.
    // Persist the native projection and optional activation first, then append
    // the marker last. A crash anywhere before it leaves the session blocked
    // for an idempotent import retry instead of exposing partial history.
    let completion_seq = existing
        .iter()
        .find(|event| event.kind == "native_session_imported")
        .map_or_else(|| store.next_seq(session.id), |event| Ok(event.seq))?;
    store.upsert_provider_session(&ProviderSessionRecord {
        id: native.id,
        unified_session_id: session.id,
        provider: native.provider.clone(),
        native_session_id: native.native_session_id.clone(),
        native_version: native.native_version.clone(),
        last_synced_seq: completion_seq,
        status: ProviderStatus::Ready,
        reset_at: None,
        capabilities: native.capabilities.clone(),
        metadata: serde_json::json!({
            "attached": true,
            "validation": "official_thread_read",
            "canonical_history_imported": true,
        }),
        created_at: now,
        updated_at: now,
    })?;
    if activate {
        store.update_session_routing(
            session.id,
            Some(&native.provider),
            "manual",
            SessionStatus::Active,
            now,
        )?;
    }
    imported_events += append_native_import_event(
        store,
        guard,
        session.id,
        None,
        &native.provider,
        &native.native_session_id,
        "import:completed",
        "native_session_imported",
        EventVisibility::User,
        &serde_json::json!({
            "provider": native.provider,
            "native_session_id": native.native_session_id,
            "turn_count": transcript.turns.len(),
            "validation": "official_thread_read",
            "canonical_history_imported": true,
        }),
        None,
        &mut imported_keys,
        &mut skipped_existing_events,
    )?;
    Ok(NativeImportReport {
        session_id: session.id,
        provider: native.provider.clone(),
        native_session_id: native.native_session_id.clone(),
        imported_turns: transcript.turns.len(),
        imported_events,
        skipped_existing_events,
        activated: activate,
        canonical_history_imported: true,
        validation: "official_thread_read",
        warning: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn append_native_import_event(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session_id: UnifiedSessionId,
    turn_id: Option<TurnId>,
    provider: &ProviderKind,
    native_session_id: &str,
    key: &str,
    kind: &str,
    visibility: EventVisibility,
    payload: &serde_json::Value,
    raw: Option<(&str, &serde_json::Value)>,
    imported_keys: &mut BTreeSet<String>,
    skipped_existing_events: &mut usize,
) -> Result<usize> {
    let key_digest = native_import_digest(&format!("{native_session_id}\0{key}"));
    if imported_keys.contains(&key_digest) {
        *skipped_existing_events = skipped_existing_events.saturating_add(1);
        return Ok(0);
    }
    let mut payload = guard.process_json(payload)?;
    let object = payload
        .as_object_mut()
        .context("native import normalized payload must be an object")?;
    object.insert(
        "native_import".to_owned(),
        serde_json::json!({
            "session_digest": native_import_digest(native_session_id),
            "key_digest": key_digest.clone(),
        }),
    );
    let raw = raw
        .map(|(raw_kind, raw)| {
            let raw = guard.process_json(raw)?;
            RawProviderEvent::new(session_id, turn_id, provider.clone(), raw_kind, raw)
                .map_err(anyhow::Error::from)
        })
        .transpose()?;
    let raw_event_id = raw.as_ref().map(|raw| raw.event_id);
    let event = CanonicalEvent {
        schema_version: 1,
        session_id,
        seq: store.next_seq(session_id)?,
        event_id: EventId::new(),
        turn_id,
        origin_provider: Some(provider.clone()),
        kind: kind.to_owned(),
        visibility,
        content_hash: canonical_content_hash(kind, visibility, &payload)?,
        payload,
        raw_event_id,
        created_at: Utc::now(),
    };
    store.append_event(&event, raw.as_ref())?;
    imported_keys.insert(key_digest);
    Ok(1)
}

#[allow(clippy::too_many_arguments)]
fn append_native_capture_event(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session_id: UnifiedSessionId,
    turn_id: TurnId,
    provider: &ProviderKind,
    native_session_id: &str,
    key: &str,
    content_digest: &str,
    kind: &str,
    visibility: EventVisibility,
    payload: &serde_json::Value,
    raw: Option<(&str, &serde_json::Value)>,
    captured_keys: &mut BTreeSet<String>,
    skipped_existing_events: &mut usize,
) -> Result<usize> {
    let key_digest = native_import_digest(&format!("{native_session_id}\0{key}"));
    if captured_keys.contains(&key_digest) {
        *skipped_existing_events = skipped_existing_events.saturating_add(1);
        return Ok(0);
    }
    let mut payload = guard.process_json(payload)?;
    let object = payload
        .as_object_mut()
        .context("native capture normalized payload must be an object")?;
    object.insert(
        "native_capture".to_owned(),
        serde_json::json!({
            "session_digest": native_import_digest(native_session_id),
            "key_digest": key_digest.clone(),
            "content_digest": content_digest,
        }),
    );
    let raw = raw
        .map(|(raw_kind, raw)| {
            let raw = guard.process_json(raw)?;
            RawProviderEvent::new(session_id, Some(turn_id), provider.clone(), raw_kind, raw)
                .map_err(anyhow::Error::from)
        })
        .transpose()?;
    let raw_event_id = raw.as_ref().map(|raw| raw.event_id);
    let event = CanonicalEvent {
        schema_version: 1,
        session_id,
        seq: store.next_seq(session_id)?,
        event_id: deterministic_capture_event_id(session_id, provider, native_session_id, key),
        turn_id: Some(turn_id),
        origin_provider: Some(provider.clone()),
        kind: kind.to_owned(),
        visibility,
        content_hash: canonical_content_hash(kind, visibility, &payload)?,
        payload,
        raw_event_id,
        created_at: Utc::now(),
    };
    store.append_event(&event, raw.as_ref())?;
    captured_keys.insert(key_digest);
    Ok(1)
}

fn deterministic_capture_event_id(
    session_id: UnifiedSessionId,
    provider: &ProviderKind,
    native_session_id: &str,
    key: &str,
) -> EventId {
    let material = format!("capture\0{session_id}\0{provider}\0{native_session_id}\0{key}");
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EventId(Uuid::from_bytes(bytes))
}

#[allow(clippy::too_many_lines)]
fn native_capture_content_digest(item: &NativeTranscriptItem) -> Result<String> {
    let public = match item {
        NativeTranscriptItem::UserPrompt {
            native_item_id,
            text,
            attachments,
            ..
        } => serde_json::json!({
            "kind": "user_prompt",
            "native_item_id": native_item_id,
            "text": text,
            "attachments": attachments,
        }),
        NativeTranscriptItem::AssistantMessage {
            native_item_id,
            text,
            final_answer,
            ..
        } => serde_json::json!({
            "kind": "assistant_message",
            "native_item_id": native_item_id,
            "text": text,
            "final_answer": final_answer,
        }),
        NativeTranscriptItem::Plan {
            native_item_id,
            text,
            ..
        } => serde_json::json!({
            "kind": "plan",
            "native_item_id": native_item_id,
            "text": text,
        }),
        NativeTranscriptItem::Command {
            native_item_id,
            command,
            cwd,
            exit_code,
            status,
            ..
        } => serde_json::json!({
            "kind": "command",
            "native_item_id": native_item_id,
            "command": command,
            "cwd": cwd,
            "exit_code": exit_code,
            "status": status,
        }),
        NativeTranscriptItem::FilesChanged {
            native_item_id,
            paths,
            status,
            ..
        } => serde_json::json!({
            "kind": "files_changed",
            "native_item_id": native_item_id,
            "paths": paths,
            "status": status,
        }),
        NativeTranscriptItem::ToolCall {
            native_item_id,
            name,
            input_summary,
            status,
            output_digest,
            artifacts,
            may_have_side_effects,
            ..
        } => serde_json::json!({
            "kind": "tool_call",
            "native_item_id": native_item_id,
            "name": name,
            "input_summary": input_summary,
            "status": status,
            "output_digest": output_digest,
            "artifacts": artifacts,
            "may_have_side_effects": may_have_side_effects,
        }),
        NativeTranscriptItem::ContextMarker {
            native_item_id,
            marker_kind,
            summary,
            content_digest,
            ..
        } => serde_json::json!({
            "kind": "context_marker",
            "native_item_id": native_item_id,
            "marker_kind": marker_kind,
            "summary": summary,
            "content_digest": content_digest,
        }),
    };
    Ok(native_import_digest(&serde_json::to_string(&public)?))
}

fn native_turn_side_effect_state(turn: &agentctl_core::NativeTranscriptTurn) -> SideEffectState {
    turn.items
        .iter()
        .fold(SideEffectState::None, |state, item| {
            let observed = match item {
                NativeTranscriptItem::Command {
                    status: NativeEffectStatus::Completed | NativeEffectStatus::Failed,
                    ..
                }
                | NativeTranscriptItem::ToolCall {
                    status: NativeEffectStatus::Completed | NativeEffectStatus::Failed,
                    may_have_side_effects: true,
                    ..
                }
                | NativeTranscriptItem::FilesChanged {
                    status: NativeEffectStatus::Failed,
                    ..
                } => SideEffectState::Possible,
                NativeTranscriptItem::FilesChanged {
                    status: NativeEffectStatus::Completed,
                    ..
                } => SideEffectState::Confirmed,
                _ => SideEffectState::None,
            };
            state.observe(observed)
        })
}

fn insert_capture_expectation(
    expectations: &mut BTreeMap<String, NativeCaptureExpectation>,
    native_session_id: &str,
    key: &str,
    content_digest: &str,
    kind: &'static str,
    turn_id: TurnId,
) -> Result<()> {
    let key_digest = native_import_digest(&format!("{native_session_id}\0{key}"));
    ensure!(
        expectations
            .insert(
                key_digest,
                NativeCaptureExpectation {
                    content_digest: content_digest.to_owned(),
                    kind,
                    turn_id,
                },
            )
            .is_none(),
        "native transcript produced a duplicate capture event key"
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn preflight_native_capture(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    native: &NativeSession,
    transcript: &NativeTranscript,
) -> Result<BTreeMap<String, NativeCaptureExpectation>> {
    let mut native_turn_ids = BTreeSet::new();
    let mut expectations = BTreeMap::new();
    for turn in &transcript.turns {
        ensure!(
            !turn.native_turn_id.is_empty() && turn.native_turn_id.len() <= 512,
            "native turn id is invalid"
        );
        ensure!(
            native_turn_ids.insert(turn.native_turn_id.as_str()),
            "native transcript repeated turn id {}",
            turn.native_turn_id
        );
        let turn_id = deterministic_import_turn_id(
            session.id,
            &native.provider,
            &native.native_session_id,
            &turn.native_turn_id,
        );
        if let Some(existing) = store.get_turn(turn_id)? {
            ensure!(
                existing.session_id == session.id
                    && existing.native_turn_id.as_deref() == Some(turn.native_turn_id.as_str())
                    && existing.provider.as_ref() == Some(&native.provider)
                    && existing.status == turn.status,
                "deterministic native capture turn collision or changed terminal status"
            );
        }
        let mut native_item_ids = BTreeSet::new();
        for item in &turn.items {
            let native_item_id = native_transcript_item_id(item);
            ensure!(
                !native_item_id.is_empty() && native_item_id.len() <= 512,
                "native item id is invalid"
            );
            ensure!(
                native_item_ids.insert(native_item_id),
                "native turn {} repeated item id {native_item_id}",
                turn.native_turn_id
            );
            guard.process_json(&serde_json::to_value(item)?)?;
            let base = format!("{}:{native_item_id}", turn.native_turn_id);
            let content_digest = native_capture_content_digest(item)?;
            match item {
                NativeTranscriptItem::UserPrompt { .. } => insert_capture_expectation(
                    &mut expectations,
                    &native.native_session_id,
                    &base,
                    &content_digest,
                    "user_prompt",
                    turn_id,
                )?,
                NativeTranscriptItem::AssistantMessage { final_answer, .. } => {
                    insert_capture_expectation(
                        &mut expectations,
                        &native.native_session_id,
                        &base,
                        &content_digest,
                        if *final_answer {
                            "assistant_final"
                        } else {
                            "assistant_text_delta"
                        },
                        turn_id,
                    )?;
                }
                NativeTranscriptItem::Plan { .. } => insert_capture_expectation(
                    &mut expectations,
                    &native.native_session_id,
                    &base,
                    &content_digest,
                    "plan_updated",
                    turn_id,
                )?,
                NativeTranscriptItem::Command { status, .. } => match status {
                    NativeEffectStatus::Declined => insert_capture_expectation(
                        &mut expectations,
                        &native.native_session_id,
                        &format!("{base}:declined"),
                        &content_digest,
                        "command_declined",
                        turn_id,
                    )?,
                    NativeEffectStatus::Completed | NativeEffectStatus::Failed => {
                        insert_capture_expectation(
                            &mut expectations,
                            &native.native_session_id,
                            &format!("{base}:started"),
                            &content_digest,
                            "command_started",
                            turn_id,
                        )?;
                        insert_capture_expectation(
                            &mut expectations,
                            &native.native_session_id,
                            &format!("{base}:completed"),
                            &content_digest,
                            "command_completed",
                            turn_id,
                        )?;
                    }
                },
                NativeTranscriptItem::FilesChanged { status, .. } => {
                    let (key, kind) = match status {
                        NativeEffectStatus::Completed => (base, "files_changed"),
                        NativeEffectStatus::Failed => {
                            (format!("{base}:failed"), "file_change_failed")
                        }
                        NativeEffectStatus::Declined => {
                            (format!("{base}:declined"), "file_change_declined")
                        }
                    };
                    insert_capture_expectation(
                        &mut expectations,
                        &native.native_session_id,
                        &key,
                        &content_digest,
                        kind,
                        turn_id,
                    )?;
                }
                NativeTranscriptItem::ToolCall { status, .. } => match status {
                    NativeEffectStatus::Declined => insert_capture_expectation(
                        &mut expectations,
                        &native.native_session_id,
                        &format!("{base}:declined"),
                        &content_digest,
                        "tool_completed",
                        turn_id,
                    )?,
                    NativeEffectStatus::Completed | NativeEffectStatus::Failed => {
                        insert_capture_expectation(
                            &mut expectations,
                            &native.native_session_id,
                            &format!("{base}:started"),
                            &content_digest,
                            "tool_started",
                            turn_id,
                        )?;
                        insert_capture_expectation(
                            &mut expectations,
                            &native.native_session_id,
                            &format!("{base}:completed"),
                            &content_digest,
                            "tool_completed",
                            turn_id,
                        )?;
                    }
                },
                NativeTranscriptItem::ContextMarker { .. } => insert_capture_expectation(
                    &mut expectations,
                    &native.native_session_id,
                    &base,
                    &content_digest,
                    "native_context_marker",
                    turn_id,
                )?,
            }
        }
        let completion_digest = native_import_digest(&serde_json::to_string(&serde_json::json!({
            "native_turn_id": turn.native_turn_id,
            "status": turn.status,
        }))?);
        insert_capture_expectation(
            &mut expectations,
            &native.native_session_id,
            &format!("{}:completed", turn.native_turn_id),
            &completion_digest,
            "turn_completed",
            turn_id,
        )?;
    }
    Ok(expectations)
}

fn native_import_digest(value: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(value.as_bytes())))
}

fn native_transcript_digest(transcript: &NativeTranscript) -> Result<String> {
    let mut turns = transcript.turns.clone();
    turns.sort_by(|left, right| left.native_turn_id.cmp(&right.native_turn_id));
    for turn in &mut turns {
        turn.items.sort_by(|left, right| {
            native_transcript_item_id(left).cmp(native_transcript_item_id(right))
        });
    }
    let stable = serde_json::json!({
        "provider": transcript.provider,
        "native_session_id": transcript.native_session_id,
        "turns": turns,
    });
    Ok(native_import_digest(&serde_json::to_string(&stable)?))
}

fn native_transcript_item_id(item: &NativeTranscriptItem) -> &str {
    match item {
        NativeTranscriptItem::UserPrompt { native_item_id, .. }
        | NativeTranscriptItem::AssistantMessage { native_item_id, .. }
        | NativeTranscriptItem::Plan { native_item_id, .. }
        | NativeTranscriptItem::Command { native_item_id, .. }
        | NativeTranscriptItem::FilesChanged { native_item_id, .. }
        | NativeTranscriptItem::ToolCall { native_item_id, .. }
        | NativeTranscriptItem::ContextMarker { native_item_id, .. } => native_item_id,
    }
}

#[allow(clippy::too_many_lines)]
fn preflight_native_import(
    store: &SqliteStore,
    guard: &PayloadGuard,
    session: &UnifiedSession,
    native: &NativeSession,
    transcript: &NativeTranscript,
) -> Result<BTreeSet<String>> {
    let mut native_turn_ids = BTreeSet::new();
    let mut event_keys = BTreeSet::new();
    for turn in &transcript.turns {
        ensure!(
            !turn.native_turn_id.is_empty() && turn.native_turn_id.len() <= 512,
            "native turn id is invalid"
        );
        ensure!(
            native_turn_ids.insert(turn.native_turn_id.as_str()),
            "native transcript repeated turn id {}",
            turn.native_turn_id
        );
        let turn_id = deterministic_import_turn_id(
            session.id,
            &native.provider,
            &native.native_session_id,
            &turn.native_turn_id,
        );
        if let Some(existing) = store.get_turn(turn_id)? {
            ensure!(
                existing.session_id == session.id
                    && existing.native_turn_id.as_deref() == Some(turn.native_turn_id.as_str())
                    && existing.provider.as_ref() == Some(&native.provider)
                    && existing.status == turn.status,
                "deterministic native import turn collision or changed terminal status"
            );
        }
        let mut native_item_ids = BTreeSet::new();
        for item in &turn.items {
            let native_item_id = match item {
                NativeTranscriptItem::UserPrompt { native_item_id, .. }
                | NativeTranscriptItem::AssistantMessage { native_item_id, .. }
                | NativeTranscriptItem::Plan { native_item_id, .. }
                | NativeTranscriptItem::Command { native_item_id, .. }
                | NativeTranscriptItem::FilesChanged { native_item_id, .. }
                | NativeTranscriptItem::ToolCall { native_item_id, .. }
                | NativeTranscriptItem::ContextMarker { native_item_id, .. } => native_item_id,
            };
            ensure!(
                !native_item_id.is_empty() && native_item_id.len() <= 512,
                "native item id is invalid"
            );
            ensure!(
                native_item_ids.insert(native_item_id.as_str()),
                "native turn {} repeated item id {native_item_id}",
                turn.native_turn_id
            );
            // Validate every provider-derived string, array, object and raw
            // fragment before the first canonical write. This makes limit or
            // redaction failures deterministic and safely retryable.
            guard.process_json(&serde_json::to_value(item)?)?;
            let base = format!("{}:{native_item_id}", turn.native_turn_id);
            let keys = match item {
                NativeTranscriptItem::Command {
                    status: NativeEffectStatus::Declined,
                    ..
                }
                | NativeTranscriptItem::FilesChanged {
                    status: NativeEffectStatus::Declined,
                    ..
                }
                | NativeTranscriptItem::ToolCall {
                    status: NativeEffectStatus::Declined,
                    ..
                } => vec![format!("{base}:declined")],
                NativeTranscriptItem::Command { .. } | NativeTranscriptItem::ToolCall { .. } => {
                    vec![format!("{base}:started"), format!("{base}:completed")]
                }
                NativeTranscriptItem::FilesChanged {
                    status: NativeEffectStatus::Failed,
                    ..
                } => vec![format!("{base}:failed")],
                _ => vec![base],
            };
            for key in keys {
                ensure!(
                    event_keys.insert(native_import_digest(&format!(
                        "{}\0{key}",
                        native.native_session_id
                    ))),
                    "native transcript produced a duplicate import event key"
                );
            }
        }
        ensure!(
            event_keys.insert(native_import_digest(&format!(
                "{}\0{}:completed",
                native.native_session_id, turn.native_turn_id
            ))),
            "native transcript produced a duplicate turn completion key"
        );
    }
    ensure!(
        event_keys.insert(native_import_digest(&format!(
            "{}\0import:completed",
            native.native_session_id
        ))),
        "native transcript produced a duplicate import completion key"
    );
    guard.process_json(&serde_json::json!({
        "provider": native.provider,
        "native_session_id": native.native_session_id,
        "turn_count": transcript.turns.len(),
        "validation": "official_thread_read",
        "canonical_history_imported": true,
    }))?;
    Ok(event_keys)
}

fn deterministic_import_turn_id(
    session_id: UnifiedSessionId,
    provider: &ProviderKind,
    native_session_id: &str,
    native_turn_id: &str,
) -> TurnId {
    let material = format!("{session_id}\0{provider}\0{native_session_id}\0{native_turn_id}");
    let digest = Sha256::digest(material.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    TurnId(Uuid::from_bytes(bytes))
}

pub fn persist_native_attachment(
    store: &SqliteStore,
    session: &UnifiedSession,
    native: &NativeSession,
    activate: bool,
) -> Result<NativeAttachmentReport> {
    ensure!(
        !native.native_session_id.is_empty() && native.native_session_id.len() <= 512,
        "native session id is invalid"
    );
    let canonical_events_before_attach = latest_seq(store, session.id)?;
    if let Some(mut existing) = store.provider_session(session.id, &native.provider)? {
        ensure!(
            existing.native_session_id == native.native_session_id,
            "{} already has native session {}; refusing to replace it because existing sync receipts belong to that projection",
            native.provider,
            existing.native_session_id
        );
        mark_native_session_attached(store, &mut existing)?;
        activate_native_attachment(store, session.id, &native.provider, activate, Utc::now())?;
        return Ok(NativeAttachmentReport {
            session_id: session.id,
            provider: native.provider.clone(),
            native_session_id: native.native_session_id.clone(),
            attached: false,
            activated: activate,
            validation: "official_resume_interface",
            canonical_history_imported: false,
            canonical_events_before_attach,
            warning: "The native transcript remains provider-owned and was not copied into the canonical history.",
        });
    }

    let now = Utc::now();
    store.upsert_provider_session(&ProviderSessionRecord {
        id: native.id,
        unified_session_id: session.id,
        provider: native.provider.clone(),
        native_session_id: native.native_session_id.clone(),
        native_version: native.native_version.clone(),
        last_synced_seq: 0,
        status: ProviderStatus::Ready,
        reset_at: None,
        capabilities: native.capabilities.clone(),
        metadata: serde_json::json!({
            "attached": true,
            "native_materialized": true,
            "native_materialized_by": "official_resume_interface",
            "validation": "official_resume_interface",
            "canonical_history_imported": false,
        }),
        created_at: now,
        updated_at: now,
    })?;
    let payload = serde_json::json!({
        "provider": native.provider,
        "native_session_id": native.native_session_id,
        "validation": "official_resume_interface",
        "canonical_history_imported": false,
        "canonical_events_before_attach": canonical_events_before_attach,
        "warning": "The native transcript remains provider-owned and was not copied into the canonical history."
    });
    let event = CanonicalEvent {
        schema_version: 1,
        session_id: session.id,
        seq: canonical_events_before_attach.saturating_add(1),
        event_id: EventId::new(),
        turn_id: None,
        origin_provider: Some(native.provider.clone()),
        kind: "native_session_attached".to_owned(),
        visibility: EventVisibility::User,
        content_hash: canonical_content_hash(
            "native_session_attached",
            EventVisibility::User,
            &payload,
        )?,
        payload,
        raw_event_id: None,
        created_at: now,
    };
    store.append_event(&event, None)?;
    activate_native_attachment(store, session.id, &native.provider, activate, now)?;
    Ok(NativeAttachmentReport {
        session_id: session.id,
        provider: native.provider.clone(),
        native_session_id: native.native_session_id.clone(),
        attached: true,
        activated: activate,
        validation: "official_resume_interface",
        canonical_history_imported: false,
        canonical_events_before_attach,
        warning: "The native transcript remains provider-owned and was not copied into the canonical history.",
    })
}

fn mark_native_session_attached(
    store: &SqliteStore,
    provider: &mut ProviderSessionRecord,
) -> Result<()> {
    let metadata = provider
        .metadata
        .as_object_mut()
        .context("provider session metadata must be a JSON object")?;
    metadata.insert("attached".to_owned(), serde_json::Value::Bool(true));
    metadata.insert(
        "native_materialized".to_owned(),
        serde_json::Value::Bool(true),
    );
    metadata.insert(
        "native_materialized_by".to_owned(),
        serde_json::Value::String("official_resume_interface".to_owned()),
    );
    metadata.remove("native_started");
    provider.updated_at = Utc::now();
    store.upsert_provider_session(provider)?;
    Ok(())
}

fn activate_native_attachment(
    store: &SqliteStore,
    session_id: UnifiedSessionId,
    provider: &ProviderKind,
    activate: bool,
    updated_at: DateTime<Utc>,
) -> Result<()> {
    if activate {
        store.update_session_routing(
            session_id,
            Some(provider),
            "manual",
            SessionStatus::Active,
            updated_at,
        )?;
    }
    Ok(())
}

fn inspect_workspace(path: &Path, recorded_fingerprint: &str) -> WorkspaceStatusView {
    if !path.exists() {
        return WorkspaceStatusView {
            exists: false,
            identity: None,
            fingerprint_changed: None,
            git: None,
            warning: Some(format!("workspace {} no longer exists", path.display())),
        };
    }
    let identity = WorkspaceIdentity::discover(path);
    let git = capture_git_snapshot(path);
    let warning = match (&identity, &git) {
        (Err(identity), Err(git)) => Some(format!(
            "workspace identity failed: {identity}; git snapshot failed: {git}"
        )),
        (Err(error), _) => Some(format!("workspace identity failed: {error}")),
        (_, Err(error)) => Some(format!("git snapshot unavailable: {error}")),
        _ => None,
    };
    let fingerprint_changed = identity
        .as_ref()
        .ok()
        .map(|identity| identity.fingerprint != recorded_fingerprint);
    WorkspaceStatusView {
        exists: true,
        identity: identity.ok(),
        fingerprint_changed,
        git: git.ok(),
        warning,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct HistoryEntry {
    pub event: CanonicalEvent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<RawProviderEvent>,
}

pub fn session_history(
    store: &SqliteStore,
    session_id: UnifiedSessionId,
    include_raw: bool,
    limit: Option<usize>,
) -> Result<Vec<HistoryEntry>> {
    let mut events = store.list_events(session_id, 0, usize::MAX)?;
    if !include_raw {
        events.retain(|event| event.visibility != EventVisibility::Internal);
    }
    if let Some(limit) = limit {
        let remove = events.len().saturating_sub(limit);
        events.drain(..remove);
    }
    events
        .into_iter()
        .map(|event| {
            let raw = if include_raw {
                event
                    .raw_event_id
                    .map(|id| store.raw_event(id))
                    .transpose()?
                    .flatten()
            } else {
                None
            };
            Ok(HistoryEntry { event, raw })
        })
        .collect()
}

fn latest_seq(store: &SqliteStore, session_id: UnifiedSessionId) -> Result<u64> {
    Ok(store.next_seq(session_id)?.saturating_sub(1))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ExportOptions {
    pub include_blobs: bool,
    pub redact: bool,
    pub include_internal: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExportReport {
    pub output: PathBuf,
    pub blob_directory: Option<PathBuf>,
    pub event_count: usize,
    pub included_blobs: usize,
    pub omitted_blobs: usize,
    pub redacted: bool,
}

pub fn export_session(
    store: &AgentctlStore,
    session: UnifiedSession,
    output: &Path,
    options: ExportOptions,
) -> Result<ExportReport> {
    let mut events = store.list_events(session.id, 0, usize::MAX)?;
    if !options.include_internal {
        events.retain(|event| event.visibility != EventVisibility::Internal);
    }
    // Raw frames live in a separate append-only table and are intentionally not
    // part of the portable canonical export format. Do not export dangling raw IDs.
    for event in &mut events {
        event.raw_event_id = None;
    }
    if options.redact {
        let redactor = Redactor::new(&RedactionConfig::with_secret_defaults())?;
        let guard = PayloadGuard::new(PayloadLimits::default(), redactor);
        for event in &mut events {
            event.payload = guard.process_json(&event.payload)?;
            event.content_hash =
                canonical_content_hash(&event.kind, event.visibility, &event.payload)?;
        }
    }

    let blob_refs = collect_blob_refs(&events);
    let blob_directory = companion_blob_dir(output);
    let temporary_blob_directory = sibling_temporary_path(&blob_directory, "blobs");
    if temporary_blob_directory.exists() {
        remove_controlled_directory(&temporary_blob_directory)?;
    }
    create_private_directory(&temporary_blob_directory)?;

    let mut export_blobs = Vec::with_capacity(blob_refs.len());
    let mut included_blobs = 0;
    for blob in blob_refs.values() {
        let include = options.include_blobs && (!options.redact || blob.redacted);
        if include {
            let bytes = store.blobs.get(blob)?;
            write_private_file(
                &temporary_blob_directory.join(blob_file_name(&blob.digest)?),
                &bytes,
            )?;
            included_blobs += 1;
        }
        export_blobs.push(ExportBlob {
            digest: blob.digest.clone(),
            size: blob.size,
            media_type: blob.media_type.clone(),
            included: include,
        });
    }

    let bundle = build_export(session, events, export_blobs, options.redact, Utc::now())?;
    let temporary_output = sibling_temporary_path(output, "export");
    let write_result = (|| -> Result<()> {
        ensure_parent_directory(output)?;
        refuse_symlink(output)?;
        let mut file = create_private_new(&temporary_output)?;
        write_jsonl(&mut file, &bundle)?;
        file.sync_all()?;
        replace_file(&temporary_output, output)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_output);
        let _ = fs::remove_dir_all(&temporary_blob_directory);
    }
    write_result?;

    let final_blob_directory = if included_blobs > 0 {
        refuse_symlink(&blob_directory)?;
        if blob_directory.exists() {
            remove_controlled_directory(&blob_directory)?;
        }
        fs::rename(&temporary_blob_directory, &blob_directory)?;
        Some(blob_directory)
    } else {
        let _ = fs::remove_dir(&temporary_blob_directory);
        if blob_directory.exists() {
            remove_controlled_directory(&blob_directory)?;
        }
        None
    };

    Ok(ExportReport {
        output: output.to_path_buf(),
        blob_directory: final_blob_directory,
        event_count: bundle.events.len(),
        included_blobs,
        omitted_blobs: blob_refs.len().saturating_sub(included_blobs),
        redacted: options.redact,
    })
}

fn collect_blob_refs(events: &[CanonicalEvent]) -> BTreeMap<String, BlobRef> {
    fn visit(value: &serde_json::Value, blobs: &mut BTreeMap<String, BlobRef>) {
        match value {
            serde_json::Value::Object(object) => {
                if object
                    .get("digest")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|digest| digest.starts_with("sha256:"))
                    && let Ok(blob) = serde_json::from_value::<BlobRef>(value.clone())
                {
                    blobs.entry(blob.digest.clone()).or_insert(blob);
                }
                for value in object.values() {
                    visit(value, blobs);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, blobs);
                }
            }
            _ => {}
        }
    }

    let mut blobs = BTreeMap::new();
    for event in events {
        visit(&event.payload, &mut blobs);
    }
    blobs
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportReport {
    pub session: UnifiedSession,
    pub event_count: usize,
    pub imported_blobs: usize,
}

pub fn import_session(store: &AgentctlStore, input: &Path) -> Result<ImportReport> {
    refuse_non_regular_file(input)?;
    let metadata = fs::metadata(input)?;
    ensure!(
        metadata.len() <= MAX_IMPORT_FILE_BYTES,
        "import exceeds {MAX_IMPORT_FILE_BYTES} bytes"
    );
    let file = File::open(input).with_context(|| format!("failed to open {}", input.display()))?;
    let mut bundle = read_jsonl(file)?;
    ensure!(
        store.get_session(bundle.manifest.session.id)?.is_none(),
        "session {} already exists",
        bundle.manifest.session.id
    );

    let blob_directory = companion_blob_dir(input);
    let imported_blobs = import_bundle_blobs(
        store,
        &bundle.manifest.blobs,
        &blob_directory,
        bundle.manifest.redacted,
    )?;

    bundle.manifest.session.active_provider = None;
    bundle.manifest.session.status = SessionStatus::Idle;
    bundle.manifest.session.parent_session_id = None;
    bundle.manifest.session.updated_at = Utc::now();
    let session = bundle.manifest.session.clone();
    store.create_session(&session)?;
    let import_result = (|| -> Result<()> {
        for event in &mut bundle.events {
            event.turn_id = None;
            event.raw_event_id = None;
            store.append_event(event, None)?;
        }
        Ok(())
    })();
    if let Err(error) = import_result {
        let _ = store.delete_session(session.id);
        return Err(error);
    }

    Ok(ImportReport {
        session,
        event_count: bundle.events.len(),
        imported_blobs,
    })
}

fn import_bundle_blobs(
    store: &AgentctlStore,
    blobs: &[ExportBlob],
    directory: &Path,
    redacted: bool,
) -> Result<usize> {
    let included = blobs
        .iter()
        .filter(|blob| blob.included)
        .collect::<Vec<_>>();
    if included.is_empty() {
        return Ok(0);
    }
    refuse_directory_symlink(directory)?;
    ensure!(directory.is_dir(), "blob companion directory is missing");
    for blob in &included {
        let path = directory.join(blob_file_name(&blob.digest)?);
        refuse_non_regular_file(&path)?;
        let metadata = fs::metadata(&path)?;
        ensure!(
            metadata.len() <= MAX_BLOB_BYTES,
            "blob {} is too large",
            blob.digest
        );
        let bytes = fs::read(&path)?;
        let stored = store.blobs.put(&bytes, blob.media_type.clone(), redacted)?;
        ensure!(
            stored.digest == blob.digest && stored.size == blob.size,
            "blob {} failed digest or size validation",
            blob.digest
        );
        store.register_blob(&stored, Utc::now())?;
    }
    Ok(included.len())
}

#[derive(Clone, Debug, Serialize)]
pub struct DeleteReport {
    pub id: UnifiedSessionId,
    pub name: String,
}

pub fn delete_session(store: &SqliteStore, session: &UnifiedSession) -> Result<DeleteReport> {
    store.delete_session(session.id)?;
    Ok(DeleteReport {
        id: session.id,
        name: session.name.clone(),
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct RetentionReport {
    pub cutoff: DateTime<Utc>,
    pub deleted_sessions: Vec<UnifiedSessionId>,
    pub deleted_blobs: Vec<String>,
    pub retained_due_to_descendants: Vec<UnifiedSessionId>,
    pub retained_due_to_native_launch: Vec<UnifiedSessionId>,
}

/// Deletes canonical sessions that have not been updated within the configured
/// retention window. A stale parent is retained while any non-expired child
/// still references it; expired descendants are deleted before their parents.
pub fn enforce_retention(
    store: &AgentctlStore,
    retention_days: u64,
    now: DateTime<Utc>,
) -> Result<RetentionReport> {
    ensure!(retention_days > 0, "retention_days must be at least 1");
    let days = i64::try_from(retention_days).context("retention_days is too large")?;
    let cutoff = now
        .checked_sub_signed(chrono::TimeDelta::days(days))
        .context("retention cutoff is outside the supported timestamp range")?;
    let mut sessions = store.list_sessions()?;
    let mut candidates = Vec::new();
    let mut retained_due_to_native_launch = Vec::new();
    for session in sessions
        .iter()
        .filter(|session| session.updated_at < cutoff)
    {
        if store.open_native_launches(session.id)?.is_empty() {
            candidates.push(session.id);
        } else {
            retained_due_to_native_launch.push(session.id);
        }
    }
    let mut deleted_sessions = Vec::new();

    while let Some((index, id)) = candidates.iter().copied().enumerate().find(|(_, id)| {
        !sessions
            .iter()
            .any(|session| session.parent_session_id == Some(*id))
    }) {
        store.delete_session(id)?;
        deleted_sessions.push(id);
        candidates.swap_remove(index);
        sessions.retain(|session| session.id != id);
    }

    candidates.sort();
    deleted_sessions.sort();
    retained_due_to_native_launch.sort();
    let mut referenced_blobs = BTreeMap::new();
    for session in store.list_sessions()? {
        referenced_blobs.extend(collect_blob_refs(&store.list_events(
            session.id,
            0,
            usize::MAX,
        )?));
    }
    let mut deleted_blobs = Vec::new();
    for blob in store.list_registered_blobs_before(cutoff)? {
        if referenced_blobs.contains_key(&blob.digest) {
            continue;
        }
        store.blobs.delete(&blob.digest)?;
        if store.unregister_blob(&blob.digest)? {
            deleted_blobs.push(blob.digest);
        }
    }
    Ok(RetentionReport {
        cutoff,
        deleted_sessions,
        deleted_blobs,
        retained_due_to_descendants: candidates,
        retained_due_to_native_launch,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct CompactionReport {
    pub checkpoint: ContextCheckpoint,
    pub projection_version: u32,
    pub retained_events: usize,
    pub omitted_events: usize,
    pub event_seq: u64,
    pub inserted: bool,
}

pub fn compact_session(
    store: &SqliteStore,
    session_id: UnifiedSessionId,
    policy: CompactionPolicy,
) -> Result<CompactionReport> {
    let events = store.list_events(session_id, 0, usize::MAX)?;
    ensure!(!events.is_empty(), "cannot compact an empty session");
    let previous_record = store.latest_checkpoint(session_id)?;
    let previous = previous_record
        .as_ref()
        .map(|record| serde_json::from_value::<ContextCheckpoint>(record.checkpoint.clone()))
        .transpose()
        .context("stored context checkpoint is invalid")?;
    let result = compact(previous.as_ref(), &events, policy)?;
    let projection_version = previous_record
        .as_ref()
        .map_or(1, |record| record.projection_version.saturating_add(1));
    let content_hash = result.checkpoint.content_hash()?;
    let record = ContextCheckpointRecord {
        id: EventId::new(),
        session_id,
        through_seq: result.checkpoint.through_seq,
        projection_version,
        checkpoint: serde_json::to_value(&result.checkpoint)?,
        content_hash,
        created_at: Utc::now(),
    };
    let inserted = store.record_checkpoint(&record)?;

    let event_seq = store.next_seq(session_id)?;
    let payload = serde_json::json!({
        "checkpoint": result.checkpoint,
        "projection_version": projection_version,
        "omitted_digests": result.omitted_digests,
    });
    let event = CanonicalEvent {
        schema_version: 1,
        session_id,
        seq: event_seq,
        event_id: EventId::new(),
        turn_id: None,
        origin_provider: None,
        kind: "context_checkpoint".into(),
        visibility: EventVisibility::Projection,
        content_hash: canonical_content_hash(
            "context_checkpoint",
            EventVisibility::Projection,
            &payload,
        )?,
        payload,
        raw_event_id: None,
        created_at: Utc::now(),
    };
    store.append_event(&event, None)?;

    Ok(CompactionReport {
        checkpoint: result.checkpoint,
        projection_version,
        retained_events: result.retained_events.len(),
        omitted_events: events.len().saturating_sub(result.retained_events.len()),
        event_seq,
        inserted,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct ForkReport {
    pub session: UnifiedSession,
    pub through_seq: u64,
    pub copied_events: usize,
}

pub fn fork_session(
    store: &SqliteStore,
    parent: &UnifiedSession,
    name: Option<&str>,
    through_seq: Option<u64>,
) -> Result<ForkReport> {
    let latest = latest_seq(store, parent.id)?;
    let through_seq = through_seq.unwrap_or(latest);
    ensure!(
        through_seq <= latest,
        "fork sequence {through_seq} exceeds latest event {latest}"
    );
    let now = Utc::now();
    let child = UnifiedSession {
        id: UnifiedSessionId::new(),
        name: name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map_or_else(|| format!("{}-fork", parent.name), ToOwned::to_owned),
        workspace_path: parent.workspace_path.clone(),
        workspace_fingerprint: parent.workspace_fingerprint.clone(),
        active_provider: None,
        routing_policy: parent.routing_policy.clone(),
        auth_mode: parent.auth_mode,
        status: SessionStatus::Active,
        parent_session_id: Some(parent.id),
        created_at: now,
        updated_at: now,
        schema_version: parent.schema_version,
    };
    let copied_events = store.fork_session(parent.id, &child, through_seq)?;
    Ok(ForkReport {
        session: child,
        through_seq,
        copied_events,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct RepairReport {
    pub integrity_before: String,
    pub integrity_after: String,
    pub indexes_rebuilt: bool,
    pub permissions_fixed: usize,
    pub projection_receipts_removed: usize,
    pub projection_pending_intents_removed: usize,
    pub projection_provider_sessions_removed: usize,
    pub blob_files_checked: usize,
    pub corrupt_blobs: Vec<String>,
    pub temporary_files_removed: usize,
    pub abandoned_native_launch: Option<NativeLaunchRecord>,
    pub unresolved_native_launches: Vec<NativeLaunchRecord>,
}

impl RepairReport {
    pub fn healthy(&self) -> bool {
        self.integrity_after == "ok"
            && self.corrupt_blobs.is_empty()
            && self.unresolved_native_launches.is_empty()
    }
}

pub fn repair_local_state(
    paths: &AgentctlPaths,
    store: &AgentctlStore,
    rebuild_projections: bool,
    abandon_native_launch: Option<Uuid>,
) -> Result<RepairReport> {
    paths.ensure()?;
    let abandonment = abandon_native_launch
        .map(|launch_id| validate_native_launch_abandonment(store, launch_id, rebuild_projections))
        .transpose()?;
    let mutation_guard = if rebuild_projections {
        Some(guard_all_session_mutations(
            paths,
            store,
            "projection rebuild",
            abandon_native_launch,
        )?)
    } else {
        None
    };
    let abandoned_native_launch = abandonment
        .as_ref()
        .map(|(launch, _, _)| abandon_native_launch_under_lease(store, launch))
        .transpose()?;
    let integrity_before = store.integrity_check()?;
    store.repair_indexes()?;
    let permissions_fixed = repair_permissions(paths)?;
    let (blob_files_checked, corrupt_blobs, temporary_files_removed) =
        inspect_and_repair_blobs(&paths.blobs)?;
    let projection_rebuild = if rebuild_projections {
        let guard = mutation_guard
            .as_ref()
            .context("projection rebuild lost its workspace mutation guard")?;
        guard.session_ids().iter().copied().try_fold(
            agentctl_storage::ProjectionRebuildResult::default(),
            |mut total, session_id| {
                store.rebuild_projection_state(session_id).map(|removed| {
                    total.provider_sessions_removed = total
                        .provider_sessions_removed
                        .saturating_add(removed.provider_sessions_removed);
                    total.receipts_removed = total
                        .receipts_removed
                        .saturating_add(removed.receipts_removed);
                    total.pending_intents_removed = total
                        .pending_intents_removed
                        .saturating_add(removed.pending_intents_removed);
                    total
                })
            },
        )?
    } else {
        agentctl_storage::ProjectionRebuildResult::default()
    };
    let integrity_after = store.integrity_check()?;
    let unresolved_native_launches = store
        .list_sessions()?
        .into_iter()
        .map(|session| store.open_native_launches(session.id))
        .collect::<agentctl_storage::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(RepairReport {
        integrity_before,
        integrity_after,
        indexes_rebuilt: true,
        permissions_fixed,
        projection_receipts_removed: projection_rebuild.receipts_removed,
        projection_pending_intents_removed: projection_rebuild.pending_intents_removed,
        projection_provider_sessions_removed: projection_rebuild.provider_sessions_removed,
        blob_files_checked,
        corrupt_blobs,
        temporary_files_removed,
        abandoned_native_launch,
        unresolved_native_launches,
    })
}

fn validate_native_launch_abandonment(
    store: &AgentctlStore,
    launch_id: Uuid,
    rebuild_projections: bool,
) -> Result<(NativeLaunchRecord, UnifiedSession, WorkspaceIdentity)> {
    let launch = store
        .native_launch(launch_id)?
        .with_context(|| format!("native launch {launch_id} does not exist"))?;
    let requires_rebuild = matches!(
        launch.state,
        NativeLaunchState::Exited | NativeLaunchState::Uncertain
    );
    ensure!(
        requires_rebuild && rebuild_projections,
        "only an exited/uncertain launch combined with --rebuild-projections can be explicitly abandoned; a started launch without a journaled PID is ambiguous and remains fail-closed"
    );
    let pid = launch.child_pid.context(
        "native launch has no journaled PID; a surviving provider process cannot be ruled out, so abandonment remains blocked",
    )?;
    ensure!(
        !crate::native::native_process_is_running(pid)?,
        "native launch process group {pid} is still alive; exit or terminate it before abandonment"
    );
    let session = store
        .get_session(launch.session_id)?
        .context("native launch canonical session no longer exists")?;
    let identity = WorkspaceIdentity::discover(&session.workspace_path)?;
    ensure!(
        identity.lease_key == launch.workspace_lease_key,
        "native launch worktree identity no longer matches its canonical session"
    );
    Ok((launch, session, identity))
}

fn abandon_native_launch_under_lease(
    store: &AgentctlStore,
    expected: &NativeLaunchRecord,
) -> Result<NativeLaunchRecord> {
    let current = store
        .native_launch(expected.id)?
        .context("native launch disappeared while acquiring its worktree lease")?;
    ensure!(
        current.state == expected.state
            && current.child_pid == expected.child_pid
            && current.updated_at == expected.updated_at,
        "native launch changed while acquiring its worktree lease; refusing abandonment"
    );
    let pid = current.child_pid.context(
        "native launch lost its journaled PID while acquiring the worktree lease; refusing abandonment",
    )?;
    ensure!(
        !crate::native::native_process_is_running(pid)?,
        "native launch process group {pid} became live while acquiring its worktree lease"
    );
    store.update_native_launch(
        current.id,
        NativeLaunchState::Failed,
        None,
        Some("explicitly abandoned after manual confirmation that no native process survived"),
        Utc::now(),
    )?;
    store
        .native_launch(current.id)?
        .context("abandoned native launch disappeared")
}

fn inspect_and_repair_blobs(root: &Path) -> Result<(usize, Vec<String>, usize)> {
    let mut checked = 0;
    let mut corrupt = Vec::new();
    let mut temporary_removed = 0;
    if !root.exists() {
        return Ok((0, corrupt, 0));
    }
    for directory in fs::read_dir(root)? {
        let directory = directory?;
        let metadata = fs::symlink_metadata(directory.path())?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            corrupt.push(directory.path().display().to_string());
            continue;
        }
        for entry in fs::read_dir(directory.path())? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                corrupt.push(path.display().to_string());
                continue;
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".tmp"))
            {
                fs::remove_file(&path)?;
                temporary_removed += 1;
                continue;
            }
            checked += 1;
            if let Err(error) = verify_compressed_blob(&path) {
                corrupt.push(format!("{}: {error:#}", path.display()));
            }
        }
    }
    Ok((checked, corrupt, temporary_removed))
}

fn verify_compressed_blob(path: &Path) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("blob filename is not UTF-8")?;
    let hash = file_name
        .strip_suffix(".zst")
        .context("blob filename does not end in .zst")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "blob filename is not a SHA-256 digest"
    );
    let parent_prefix = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .context("blob prefix directory is invalid")?;
    ensure!(
        parent_prefix == &hash[..2],
        "blob is in the wrong prefix directory"
    );
    let file = File::open(path)?;
    let decoder = zstd::stream::read::Decoder::new(file)?;
    let mut bytes = Vec::new();
    decoder
        .take(MAX_BLOB_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BLOB_BYTES,
        "blob expands beyond limit"
    );
    let actual = hex::encode(Sha256::digest(&bytes));
    ensure!(actual.eq_ignore_ascii_case(hash), "blob digest mismatch");
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub manifest_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginInstallReport {
    pub plugin: InstalledPlugin,
    pub updated: bool,
}

pub fn install_plugin(
    paths: &AgentctlPaths,
    source_manifest: &Path,
) -> Result<PluginInstallReport> {
    refuse_non_regular_file(source_manifest)?;
    let text = fs::read_to_string(source_manifest)
        .with_context(|| format!("failed to read {}", source_manifest.display()))?;
    ensure!(text.len() <= 1024 * 1024, "plugin manifest exceeds 1 MiB");
    let mut manifest = PluginManifest::from_toml(&text)?;
    validate_plugin_environment(&manifest)?;
    let source_parent = source_manifest.parent().unwrap_or_else(|| Path::new("."));
    let executable = if manifest.executable.is_absolute() {
        manifest.executable.clone()
    } else {
        source_parent.join(&manifest.executable)
    };
    manifest.executable = fs::canonicalize(&executable)
        .with_context(|| format!("plugin executable {} does not exist", executable.display()))?;
    validate_plugin_executable(&manifest.executable)?;
    ensure!(
        manifest
            .protocol_versions
            .contains(&CURRENT_PROTOCOL_VERSION),
        "plugin does not support protocol version {CURRENT_PROTOCOL_VERSION}"
    );

    let target_directory = paths.plugins.join(&manifest.name);
    refuse_directory_symlink(&target_directory)?;
    create_private_directory(&target_directory)?;
    let target = target_directory.join(PLUGIN_MANIFEST_FILE);
    let updated = target.exists();
    write_private_atomic(&target, manifest.to_toml()?.as_bytes())?;
    Ok(PluginInstallReport {
        plugin: InstalledPlugin {
            manifest,
            manifest_path: target,
        },
        updated,
    })
}

pub fn list_plugins(paths: &AgentctlPaths) -> Result<Vec<InstalledPlugin>> {
    let mut plugins = Vec::new();
    for name in installed_plugin_names(paths)? {
        plugins.push(load_installed_plugin(paths, &name)?);
    }
    plugins.sort_by(|left, right| left.manifest.name.cmp(&right.manifest.name));
    Ok(plugins)
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginRemoveReport {
    pub name: String,
    pub removed: bool,
}

pub fn remove_plugin(paths: &AgentctlPaths, name: &str) -> Result<PluginRemoveReport> {
    validate_plugin_name(name)?;
    let directory = paths.plugins.join(name);
    if !directory.exists() {
        return Ok(PluginRemoveReport {
            name: name.into(),
            removed: false,
        });
    }
    refuse_directory_symlink(&directory)?;
    fs::remove_dir_all(&directory)?;
    Ok(PluginRemoveReport {
        name: name.into(),
        removed: true,
    })
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginDoctorState {
    Compatible,
    Incompatible,
    Invalid,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginDoctorEntry {
    pub name: String,
    pub state: PluginDoctorState,
    pub manifest_path: PathBuf,
    pub executable: Option<PathBuf>,
    pub capabilities: Option<PluginCapabilities>,
    pub detail: String,
}

pub fn doctor_plugins(
    paths: &AgentctlPaths,
    selected_name: Option<&str>,
) -> Result<Vec<PluginDoctorEntry>> {
    let names = if let Some(name) = selected_name {
        validate_plugin_name(name)?;
        vec![name.to_owned()]
    } else {
        installed_plugin_names(paths)?
    };
    Ok(names
        .into_iter()
        .map(|name| inspect_installed_plugin(paths, &name))
        .collect())
}

fn inspect_installed_plugin(paths: &AgentctlPaths, name: &str) -> PluginDoctorEntry {
    let manifest_path = paths.plugins.join(name).join(PLUGIN_MANIFEST_FILE);
    match load_installed_plugin(paths, name) {
        Ok(plugin) => {
            let protocol_ok = plugin
                .manifest
                .protocol_versions
                .contains(&CURRENT_PROTOCOL_VERSION);
            let executable_result = validate_plugin_executable(&plugin.manifest.executable);
            match (protocol_ok, executable_result) {
                (true, Ok(())) => PluginDoctorEntry {
                    name: name.into(),
                    state: PluginDoctorState::Compatible,
                    manifest_path,
                    executable: Some(plugin.manifest.executable),
                    capabilities: Some(plugin.manifest.capabilities),
                    detail: format!("supports plugin protocol {CURRENT_PROTOCOL_VERSION}"),
                },
                (false, _) => PluginDoctorEntry {
                    name: name.into(),
                    state: PluginDoctorState::Incompatible,
                    manifest_path,
                    executable: Some(plugin.manifest.executable),
                    capabilities: Some(plugin.manifest.capabilities),
                    detail: format!(
                        "protocol versions {:?} do not include {CURRENT_PROTOCOL_VERSION}",
                        plugin.manifest.protocol_versions
                    ),
                },
                (_, Err(error)) => PluginDoctorEntry {
                    name: name.into(),
                    state: PluginDoctorState::Invalid,
                    manifest_path,
                    executable: Some(plugin.manifest.executable),
                    capabilities: Some(plugin.manifest.capabilities),
                    detail: error.to_string(),
                },
            }
        }
        Err(error) => PluginDoctorEntry {
            name: name.into(),
            state: PluginDoctorState::Invalid,
            manifest_path,
            executable: None,
            capabilities: None,
            detail: error.to_string(),
        },
    }
}

fn installed_plugin_names(paths: &AgentctlPaths) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(&paths.plugins)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("plugin directory name is not UTF-8"))?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn load_installed_plugin(paths: &AgentctlPaths, name: &str) -> Result<InstalledPlugin> {
    validate_plugin_name(name)?;
    let directory = paths.plugins.join(name);
    refuse_directory_symlink(&directory)?;
    let manifest_path = directory.join(PLUGIN_MANIFEST_FILE);
    refuse_non_regular_file(&manifest_path)?;
    let manifest = PluginManifest::from_toml(&fs::read_to_string(&manifest_path)?)?;
    ensure!(
        manifest.name == name,
        "plugin directory and manifest name differ"
    );
    validate_plugin_environment(&manifest)?;
    Ok(InstalledPlugin {
        manifest,
        manifest_path,
    })
}

fn validate_plugin_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    ensure!(
        characters
            .next()
            .is_some_and(|character| character.is_ascii_lowercase())
            && characters.all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '-' | '_')
            }),
        "invalid plugin name"
    );
    Ok(())
}

fn validate_plugin_environment(manifest: &PluginManifest) -> Result<()> {
    for key in manifest.env.keys() {
        let normalized = key.to_ascii_lowercase();
        ensure!(
            ![
                "token",
                "secret",
                "password",
                "api_key",
                "apikey",
                "credential"
            ]
            .iter()
            .any(|needle| normalized.contains(needle)),
            "plugin manifest cannot persist a potentially secret environment variable: {key}"
        );
    }
    Ok(())
}

fn validate_plugin_executable(path: &Path) -> Result<()> {
    refuse_non_regular_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            fs::metadata(path)?.permissions().mode() & 0o111 != 0,
            "plugin executable {} is not executable",
            path.display()
        );
    }
    Ok(())
}

fn companion_blob_dir(output: &Path) -> PathBuf {
    let mut value: OsString = output.as_os_str().to_owned();
    value.push(".blobs");
    PathBuf::from(value)
}

fn blob_file_name(digest: &str) -> Result<String> {
    let hash = digest
        .strip_prefix("sha256:")
        .context("blob digest does not use sha256")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid blob digest"
    );
    Ok(hash.to_ascii_lowercase())
}

fn sibling_temporary_path(path: &Path, label: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(label);
    parent.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ))
}

fn ensure_parent_directory(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    set_private_directory(path)
}

fn create_private_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_parent_directory(path)?;
    refuse_symlink(path)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    set_private_file(path)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_parent_directory(path)?;
    refuse_symlink(path)?;
    let temporary = sibling_temporary_path(path, "state");
    let result = (|| -> Result<()> {
        let mut file = create_private_new(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)?;
    set_private_file(destination)
}

fn refuse_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "refusing symlink {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn refuse_non_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{} is not a regular non-symlink file",
        path.display()
    );
    Ok(())
}

fn refuse_directory_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{} is not a real directory",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn remove_controlled_directory(path: &Path) -> Result<()> {
    refuse_directory_symlink(path)?;
    fs::remove_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn repair_permissions(paths: &AgentctlPaths) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let mut fixed = 0usize;
    for directory in [
        &paths.home,
        &paths.blobs,
        &paths.protocols,
        &paths.plugins,
        &paths.logs,
        &paths.locks,
    ] {
        if directory.exists() && fs::metadata(directory)?.permissions().mode() & 0o777 != 0o700 {
            set_private_directory(directory)?;
            fixed += 1;
        }
    }
    for file in [&paths.database, &paths.config_file] {
        if file.exists() && fs::metadata(file)?.permissions().mode() & 0o777 != 0o600 {
            set_private_file(file)?;
            fixed += 1;
        }
    }
    for suffix in ["-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", paths.database.display()));
        if file.exists() && fs::metadata(&file)?.permissions().mode() & 0o777 != 0o600 {
            set_private_file(&file)?;
            fixed += 1;
        }
    }
    for root in [
        &paths.blobs,
        &paths.protocols,
        &paths.plugins,
        &paths.logs,
        &paths.locks,
    ] {
        fixed = fixed.saturating_add(repair_tree_permissions(root)?);
    }
    Ok(fixed)
}

#[cfg(unix)]
fn repair_tree_permissions(root: &Path) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;
    let mut fixed = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if metadata.permissions().mode() & 0o777 != 0o700 {
                set_private_directory(&path)?;
                fixed += 1;
            }
            fixed = fixed.saturating_add(repair_tree_permissions(&path)?);
        } else if metadata.is_file() && metadata.permissions().mode() & 0o777 != 0o600 {
            set_private_file(&path)?;
            fixed += 1;
        }
    }
    Ok(fixed)
}

#[cfg(not(unix))]
fn repair_permissions(_paths: &AgentctlPaths) -> Result<usize> {
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agentctl_core::{AuthMode, ProviderKind, ProviderSessionId, UsageSnapshot};
    use agentctl_storage::{NativeLaunchRecord, NativeLaunchState};

    use super::*;

    fn fixture() -> (
        tempfile::TempDir,
        AgentctlPaths,
        AgentctlStore,
        UnifiedSession,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let paths = AgentctlPaths::resolve(Some(directory.path().join("home"))).unwrap();
        let store = open_store(&paths).unwrap();
        let workspace = directory.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let now = Utc::now();
        let session = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "fixture".into(),
            workspace_path: workspace,
            workspace_fingerprint: "sha256:fixture".into(),
            active_provider: Some(ProviderKind::Codex),
            routing_policy: "sticky-balanced".into(),
            auth_mode: AuthMode::NativeLocal,
            status: SessionStatus::Active,
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            schema_version: 1,
        };
        store.create_session(&session).unwrap();
        append_event(
            &store,
            session.id,
            1,
            "user_prompt",
            serde_json::json!({"text":"token=secret-value"}),
        );
        append_event(
            &store,
            session.id,
            2,
            "assistant_final",
            serde_json::json!({"text":"done","decisions":["use sqlite"]}),
        );
        (directory, paths, store, session)
    }

    fn append_event(
        store: &SqliteStore,
        session_id: UnifiedSessionId,
        seq: u64,
        kind: &str,
        payload: serde_json::Value,
    ) {
        let event = CanonicalEvent {
            schema_version: 1,
            session_id,
            seq,
            event_id: EventId::new(),
            turn_id: None,
            origin_provider: None,
            kind: kind.into(),
            visibility: EventVisibility::User,
            content_hash: canonical_content_hash(kind, EventVisibility::User, &payload).unwrap(),
            payload,
            raw_event_id: None,
            created_at: Utc::now(),
        };
        store.append_event(&event, None).unwrap();
    }

    fn record_native_snapshot(
        store: &SqliteStore,
        session: &UnifiedSession,
        phase: &str,
        digest: &str,
        created_at: DateTime<Utc>,
    ) {
        let snapshot = GitSnapshot {
            root: session.workspace_path.clone(),
            head: Some("deadbeef".to_owned()),
            branch: Some("main".to_owned()),
            changed_paths: Vec::new(),
            dirty: false,
            diff_digest: digest.to_owned(),
            coverage_complete: true,
            captured_at: created_at,
        };
        store
            .record_workspace_snapshot(&agentctl_storage::WorkspaceSnapshotRecord {
                id: EventId::new(),
                session_id: session.id,
                turn_id: None,
                phase: phase.to_owned(),
                fingerprint: session.workspace_fingerprint.clone(),
                snapshot: serde_json::to_value(snapshot).unwrap(),
                diff_digest: Some(digest.to_owned()),
                created_at,
            })
            .unwrap();
    }

    #[test]
    fn crashed_native_failover_is_a_deterministic_non_replay_continuation() {
        let (_directory, _paths, store, session) = fixture();
        let identity = WorkspaceIdentity::discover(&session.workspace_path).unwrap();
        let started_at = Utc::now();
        record_native_snapshot(
            &store,
            &session,
            "before_native",
            "sha256:before",
            started_at - chrono::TimeDelta::milliseconds(1),
        );
        record_native_snapshot(
            &store,
            &session,
            "after_native_recovery",
            "sha256:after",
            started_at + chrono::TimeDelta::milliseconds(1),
        );
        let launch = NativeLaunchRecord {
            id: Uuid::now_v7(),
            session_id: session.id,
            provider: ProviderKind::Claude,
            native_session_id: Uuid::new_v4().to_string(),
            workspace_lease_key: identity.lease_key,
            child_pid: Some(424_242),
            state: NativeLaunchState::Uncertain,
            exit_code: Some(1),
            error: Some("simulated crash".to_owned()),
            metadata: serde_json::json!({}),
            started_at,
            updated_at: started_at,
        };
        store
            .start_native_launch(&NativeLaunchRecord {
                state: NativeLaunchState::Started,
                child_pid: None,
                error: None,
                ..launch.clone()
            })
            .unwrap();
        store.record_native_launch_pid(launch.id, 424_242).unwrap();
        store
            .update_native_launch(
                launch.id,
                NativeLaunchState::Uncertain,
                launch.exit_code,
                launch.error.as_deref(),
                Utc::now(),
            )
            .unwrap();
        let launch = store.native_launch(launch.id).unwrap().unwrap();
        let guard = PayloadGuard::new(PayloadLimits::default(), Redactor::default());

        let first =
            persist_native_failover_continuation(&store, &guard, &session, &launch).unwrap();
        let repeated =
            persist_native_failover_continuation(&store, &guard, &session, &launch).unwrap();

        assert_eq!(first.event_id, repeated.event_id);
        assert_eq!(first.seq, repeated.seq);
        assert_eq!(first.side_effect_state, SideEffectState::Confirmed);
        let markers = store
            .list_events(session.id, 0, usize::MAX)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "native_context_marker")
            .collect::<Vec<_>>();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].payload["replay_allowed"], false);
        assert_eq!(markers[0].payload["continuation"], true);
        let capsule = agentctl_transcript::HandoffCapsule::from_events(
            session.id,
            &ProviderKind::Claude,
            &markers,
        )
        .unwrap()
        .render_xml();
        assert!(capsule.contains("Do not replay the original request"));
    }

    #[test]
    fn crashed_native_failover_without_post_snapshot_stays_fail_closed() {
        let (_directory, _paths, store, session) = fixture();
        let identity = WorkspaceIdentity::discover(&session.workspace_path).unwrap();
        let started_at = Utc::now();
        record_native_snapshot(
            &store,
            &session,
            "before_native",
            "sha256:before",
            started_at - chrono::TimeDelta::milliseconds(1),
        );
        let launch = NativeLaunchRecord {
            id: Uuid::now_v7(),
            session_id: session.id,
            provider: ProviderKind::Codex,
            native_session_id: "thread-crashed".to_owned(),
            workspace_lease_key: identity.lease_key,
            child_pid: Some(424_242),
            state: NativeLaunchState::Uncertain,
            exit_code: Some(1),
            error: Some("simulated crash".to_owned()),
            metadata: serde_json::json!({}),
            started_at,
            updated_at: started_at,
        };
        let guard = PayloadGuard::new(PayloadLimits::default(), Redactor::default());

        let error = persist_native_failover_continuation(&store, &guard, &session, &launch)
            .unwrap_err()
            .to_string();

        assert!(error.contains("post-crash workspace snapshot"));
        assert!(
            store
                .list_events(session.id, 0, usize::MAX)
                .unwrap()
                .iter()
                .all(|event| event.kind != "native_context_marker")
        );
    }

    fn journal_native_launch(
        store: &SqliteStore,
        session: &UnifiedSession,
        state: NativeLaunchState,
    ) -> Uuid {
        let identity = WorkspaceIdentity::discover(&session.workspace_path).unwrap();
        let id = Uuid::now_v7();
        let now = Utc::now();
        store
            .start_native_launch(&NativeLaunchRecord {
                id,
                session_id: session.id,
                provider: ProviderKind::Codex,
                native_session_id: format!("native-{id}"),
                workspace_lease_key: identity.lease_key,
                child_pid: None,
                state: NativeLaunchState::Started,
                exit_code: None,
                error: None,
                metadata: serde_json::json!({}),
                started_at: now,
                updated_at: now,
            })
            .unwrap();
        if state != NativeLaunchState::Started {
            store
                .update_native_launch(id, state, None, None, Utc::now())
                .unwrap();
        }
        id
    }

    #[test]
    fn resolves_by_uuid_name_and_current_workspace() {
        let (_directory, _paths, store, session) = fixture();
        assert_eq!(
            resolve_session(&store, Some(&session.id.to_string()), Path::new("/"))
                .unwrap()
                .id,
            session.id
        );
        assert_eq!(
            resolve_session(&store, Some("fixture"), Path::new("/"))
                .unwrap()
                .id,
            session.id
        );
        assert_eq!(
            resolve_session(&store, None, &session.workspace_path)
                .unwrap()
                .id,
            session.id
        );
    }

    #[test]
    fn explicit_repair_rejects_ambiguous_started_launches_and_rebuilds_uncertain_ones() {
        let (_directory, paths, store, session) = fixture();
        let launch_id = journal_native_launch(&store, &session, NativeLaunchState::Started);
        assert!(repair_local_state(&paths, &store, false, Some(launch_id)).is_err());
        assert!(repair_local_state(&paths, &store, true, Some(launch_id)).is_err());
        store
            .update_native_launch(
                launch_id,
                NativeLaunchState::Failed,
                None,
                Some("test cleanup"),
                Utc::now(),
            )
            .unwrap();

        let journaled = journal_native_launch(&store, &session, NativeLaunchState::Started);
        store.record_native_launch_pid(journaled, 424_242).unwrap();
        assert!(repair_local_state(&paths, &store, false, Some(journaled)).is_err());
        store
            .update_native_launch(
                journaled,
                NativeLaunchState::Failed,
                None,
                Some("test cleanup"),
                Utc::now(),
            )
            .unwrap();

        let ambiguous = journal_native_launch(&store, &session, NativeLaunchState::Uncertain);
        assert!(repair_local_state(&paths, &store, false, Some(ambiguous)).is_err());
        assert!(repair_local_state(&paths, &store, true, Some(ambiguous)).is_err());
        store
            .update_native_launch(
                ambiguous,
                NativeLaunchState::Failed,
                None,
                Some("test cleanup"),
                Utc::now(),
            )
            .unwrap();

        let uncertain = journal_native_launch(&store, &session, NativeLaunchState::Started);
        store
            .record_native_launch_pid(uncertain, 2_000_000_000)
            .unwrap();
        store
            .update_native_launch(
                uncertain,
                NativeLaunchState::Uncertain,
                None,
                Some("simulated post-spawn failure"),
                Utc::now(),
            )
            .unwrap();
        assert!(repair_local_state(&paths, &store, false, Some(uncertain)).is_err());
        assert_eq!(
            repair_local_state(&paths, &store, true, Some(uncertain))
                .unwrap()
                .abandoned_native_launch
                .unwrap()
                .state,
            NativeLaunchState::Failed
        );
    }

    #[test]
    fn list_status_history_and_delete_reflect_canonical_state() {
        let (_directory, _paths, store, session) = fixture();
        let listed = list_sessions(&store).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].latest_seq, 2);

        let status = session_status(&store, session.clone()).unwrap();
        assert_eq!(status.latest_seq, 2);
        assert!(status.workspace.exists);
        let history = session_history(&store, session.id, false, Some(1)).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].event.kind, "assistant_final");

        let deleted = delete_session(&store, &session).unwrap();
        assert_eq!(deleted.id, session.id);
        assert!(store.get_session(session.id).unwrap().is_none());
    }

    #[test]
    fn session_mutations_reject_every_open_native_launch_state() {
        for state in [
            NativeLaunchState::Started,
            NativeLaunchState::CaptureReady,
            NativeLaunchState::Exited,
            NativeLaunchState::Uncertain,
        ] {
            let (_directory, paths, store, session) = fixture();
            let launch_id = journal_native_launch(&store, &session, state);
            let error = guard_session_mutation(&paths, &store, &session, "test mutation")
                .unwrap_err()
                .to_string();
            assert!(error.contains("test mutation refused"));
            assert!(error.contains(&launch_id.to_string()));
            assert!(error.contains(&format!("{state:?}")));
        }
    }

    #[test]
    fn session_mutation_guard_covers_same_worktree_and_live_file_lock() {
        let (_directory, paths, store, session) = fixture();
        let identity = WorkspaceIdentity::discover(&session.workspace_path).unwrap();
        let mut sibling = session.clone();
        sibling.id = UnifiedSessionId::new();
        sibling.name = "same-worktree".to_owned();
        sibling.created_at = Utc::now();
        sibling.updated_at = sibling.created_at;
        store.create_session(&sibling).unwrap();
        journal_native_launch(&store, &session, NativeLaunchState::Started);
        assert!(
            guard_session_mutation(&paths, &store, &sibling, "same worktree mutation").is_err()
        );

        let launch = store.open_native_launches(session.id).unwrap().remove(0);
        store
            .update_native_launch(
                launch.id,
                NativeLaunchState::Captured,
                Some(0),
                None,
                Utc::now(),
            )
            .unwrap();
        let _live_lease = WorkspaceLease::acquire_for_identity(
            &paths.locks,
            &identity,
            session.id,
            TurnId::new(),
        )
        .unwrap();
        let error = guard_session_mutation(&paths, &store, &session, "live mutation")
            .unwrap_err()
            .to_string();
        assert!(error.contains("live mutation refused"));
        assert!(error.contains("another agentctl process owns worktree"));
    }

    #[test]
    fn native_attachment_is_audited_without_claiming_transcript_import() {
        let (_directory, _paths, store, session) = fixture();
        let native = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Codex,
            native_session_id: "thread-existing".to_owned(),
            native_version: Some("1.0".to_owned()),
            capabilities: BTreeMap::from([("resume".to_owned(), true)]),
        };
        let report = persist_native_attachment(&store, &session, &native, true).unwrap();
        assert!(report.attached);
        assert!(report.activated);
        assert!(!report.canonical_history_imported);
        assert_eq!(report.canonical_events_before_attach, 2);

        let stored = store
            .provider_session(session.id, &ProviderKind::Codex)
            .unwrap()
            .unwrap();
        assert_eq!(stored.native_session_id, "thread-existing");
        assert_eq!(stored.last_synced_seq, 0);
        assert_eq!(stored.metadata["native_materialized"], true);
        let event = store.list_events(session.id, 2, 10).unwrap().remove(0);
        assert_eq!(event.kind, "native_session_attached");
        assert_eq!(event.payload["canonical_history_imported"], false);

        let repeated = persist_native_attachment(&store, &session, &native, false).unwrap();
        assert!(!repeated.attached);
        let conflicting = NativeSession {
            native_session_id: "different-thread".to_owned(),
            ..native
        };
        assert!(persist_native_attachment(&store, &session, &conflicting, false).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn codex_native_history_import_is_canonical_raw_separated_and_idempotent() {
        let (_directory, _paths, store, populated) = fixture();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&populated.workspace_path)
                .status()
                .unwrap()
                .success()
        );
        let native_cwd = populated.workspace_path.join("nested/cwd");
        std::fs::create_dir_all(&native_cwd).unwrap();
        let now = Utc::now();
        let session = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "native-import".to_owned(),
            active_provider: None,
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            ..populated.clone()
        };
        store.create_session(&session).unwrap();
        let native = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Codex,
            native_session_id: "thr_import".to_owned(),
            native_version: Some("1.0".to_owned()),
            capabilities: BTreeMap::from([("thread_read".to_owned(), true)]),
        };
        let transcript = NativeTranscript {
            provider: ProviderKind::Codex,
            native_session_id: "thr_import".to_owned(),
            workspace_cwd: native_cwd,
            turns: vec![agentctl_core::NativeTranscriptTurn {
                native_turn_id: "turn_native_1".to_owned(),
                status: TurnStatus::Completed,
                items: vec![
                    NativeTranscriptItem::UserPrompt {
                        native_item_id: "user_1".to_owned(),
                        text: "fix it".to_owned(),
                        attachments: Vec::new(),
                        raw: serde_json::json!({"type": "userMessage", "text": "fix it"}),
                    },
                    NativeTranscriptItem::Command {
                        native_item_id: "command_1".to_owned(),
                        command: "cargo test".to_owned(),
                        cwd: Some("/repo".to_owned()),
                        exit_code: Some(0),
                        status: NativeEffectStatus::Completed,
                        raw: serde_json::json!({"type": "commandExecution", "command": "cargo test"}),
                    },
                    NativeTranscriptItem::AssistantMessage {
                        native_item_id: "assistant_1".to_owned(),
                        text: "done".to_owned(),
                        final_answer: true,
                        raw: serde_json::json!({"type": "agentMessage", "text": "done"}),
                    },
                    NativeTranscriptItem::ToolCall {
                        native_item_id: "web_1".to_owned(),
                        name: "web.search".to_owned(),
                        input_summary: serde_json::json!({
                            "shape": "object",
                            "keys": ["query"],
                            "item_count": 1,
                            "size_bytes": 20,
                            "digest": "sha256:web-input"
                        }),
                        status: NativeEffectStatus::Completed,
                        output_digest: None,
                        artifacts: Vec::new(),
                        may_have_side_effects: false,
                        raw: serde_json::json!({
                            "type": "webSearch",
                            "name": "web.search"
                        }),
                    },
                    NativeTranscriptItem::ContextMarker {
                        native_item_id: "compact_1".to_owned(),
                        marker_kind: "context_compaction".to_owned(),
                        summary: "Codex compacted its native context.".to_owned(),
                        content_digest: None,
                        raw: serde_json::json!({"type": "contextCompaction"}),
                    },
                ],
            }],
        };
        let guard = crate::config::Config {
            redaction_patterns: vec!["thr_import|turn_native_1|sha256:[0-9a-f]+".to_owned()],
            ..crate::config::Config::default()
        }
        .payload_guard()
        .unwrap();
        let first =
            persist_native_import(&store, &guard, &session, &native, &transcript, true).unwrap();
        assert!(first.canonical_history_imported);
        assert_eq!(first.imported_turns, 1);
        assert!(first.imported_events >= 6);
        let events = store.list_events(session.id, 0, usize::MAX).unwrap();
        assert!(events.iter().any(|event| event.kind == "user_prompt"));
        assert!(events.iter().any(|event| event.kind == "assistant_final"));
        assert!(events.iter().any(|event| event.kind == "tool_started"));
        assert!(events.iter().any(|event| event.kind == "tool_completed"));
        assert!(
            events
                .iter()
                .any(|event| event.kind == "native_context_marker")
        );
        assert!(events.iter().any(|event| event.raw_event_id.is_some()));
        for digest in events.iter().filter_map(|event| {
            event
                .payload
                .pointer("/native_import/key_digest")
                .and_then(serde_json::Value::as_str)
        }) {
            assert!(digest.starts_with("sha256:"));
            assert_ne!(digest, "[REDACTED]");
        }
        let provider = store
            .provider_session(session.id, &ProviderKind::Codex)
            .unwrap()
            .unwrap();
        assert_eq!(
            provider.last_synced_seq,
            events.last().map(|event| event.seq).unwrap()
        );

        let mut reordered = transcript.clone();
        reordered.turns[0].items.reverse();
        let repeated =
            persist_native_import(&store, &guard, &session, &native, &reordered, false).unwrap();
        assert_eq!(repeated.imported_events, 0);
        assert!(repeated.skipped_existing_events >= first.imported_events);
        assert_eq!(
            store.list_events(session.id, 0, usize::MAX).unwrap().len(),
            events.len()
        );
        let mut changed_after_commit = transcript.clone();
        changed_after_commit.turns[0]
            .items
            .push(NativeTranscriptItem::Plan {
                native_item_id: "late_plan".to_owned(),
                text: "new external history".to_owned(),
                raw: serde_json::json!({"type": "plan", "text": "new external history"}),
            });
        assert!(
            persist_native_import(
                &store,
                &guard,
                &session,
                &native,
                &changed_after_commit,
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("changed after its canonical import was committed")
        );
        assert_eq!(
            store.list_events(session.id, 0, usize::MAX).unwrap().len(),
            events.len()
        );
        let mut changed_content_same_ids = transcript.clone();
        let NativeTranscriptItem::AssistantMessage { text, .. } =
            &mut changed_content_same_ids.turns[0].items[2]
        else {
            unreachable!();
        };
        *text = "different result under the same native item id".to_owned();
        assert!(
            persist_native_import(
                &store,
                &guard,
                &session,
                &native,
                &changed_content_same_ids,
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("changed after its canonical import was committed")
        );
        assert_eq!(
            store.list_events(session.id, 0, usize::MAX).unwrap().len(),
            events.len()
        );

        let mut partial_session = session.clone();
        partial_session.id = UnifiedSessionId::new();
        partial_session.name = "partial-import".to_owned();
        store.create_session(&partial_session).unwrap();
        let partial_digest = native_transcript_digest(&transcript).unwrap();
        let partial_key = format!("import:started:{partial_digest}");
        let mut partial_keys = BTreeSet::new();
        let mut partial_skipped = 0;
        append_native_import_event(
            &store,
            &guard,
            partial_session.id,
            None,
            &native.provider,
            &native.native_session_id,
            &partial_key,
            "native_session_import_started",
            EventVisibility::Internal,
            &serde_json::json!({"turn_count": transcript.turns.len()}),
            None,
            &mut partial_keys,
            &mut partial_skipped,
        )
        .unwrap();
        let mut changed_during_partial = transcript.clone();
        let NativeTranscriptItem::AssistantMessage { text, .. } =
            &mut changed_during_partial.turns[0].items[2]
        else {
            unreachable!();
        };
        *text = "changed before retry".to_owned();
        assert!(
            persist_native_import(
                &store,
                &guard,
                &partial_session,
                &native,
                &changed_during_partial,
                false,
            )
            .unwrap_err()
            .to_string()
            .contains("changed while a prior canonical import was incomplete")
        );
        assert_eq!(
            store
                .list_events(partial_session.id, 0, usize::MAX)
                .unwrap()
                .len(),
            1
        );

        let other_workspace = tempfile::tempdir().unwrap();
        let mut wrong_workspace = transcript.clone();
        wrong_workspace.workspace_cwd = other_workspace.path().to_path_buf();
        assert!(
            persist_native_import(&store, &guard, &session, &native, &wrong_workspace, false,)
                .unwrap_err()
                .to_string()
                .contains("does not match canonical workspace")
        );

        let mut oversized_session = session.clone();
        oversized_session.id = UnifiedSessionId::new();
        oversized_session.name = "oversized-import".to_owned();
        store.create_session(&oversized_session).unwrap();
        let mut oversized = transcript.clone();
        let NativeTranscriptItem::UserPrompt { text, .. } = &mut oversized.turns[0].items[0] else {
            unreachable!();
        };
        *text = "x".repeat(guard.limits().max_string_bytes + 1);
        assert!(
            persist_native_import(
                &store,
                &guard,
                &oversized_session,
                &native,
                &oversized,
                false,
            )
            .is_err()
        );
        assert!(
            store
                .list_events(oversized_session.id, 0, usize::MAX)
                .unwrap()
                .is_empty()
        );
        let oversized_turn_id = deterministic_import_turn_id(
            oversized_session.id,
            &native.provider,
            &native.native_session_id,
            &oversized.turns[0].native_turn_id,
        );
        assert!(store.get_turn(oversized_turn_id).unwrap().is_none());

        let mut declined_session = session.clone();
        declined_session.id = UnifiedSessionId::new();
        declined_session.name = "declined-effects".to_owned();
        store.create_session(&declined_session).unwrap();
        let declined = NativeTranscript {
            provider: ProviderKind::Codex,
            native_session_id: native.native_session_id.clone(),
            workspace_cwd: declined_session.workspace_path.clone(),
            turns: vec![agentctl_core::NativeTranscriptTurn {
                native_turn_id: "turn_declined".to_owned(),
                status: TurnStatus::Completed,
                items: vec![
                    NativeTranscriptItem::Command {
                        native_item_id: "command_declined".to_owned(),
                        command: "rm important.txt".to_owned(),
                        cwd: Some(declined_session.workspace_path.display().to_string()),
                        exit_code: None,
                        status: NativeEffectStatus::Declined,
                        raw: serde_json::json!({"type": "commandExecution", "status": "declined"}),
                    },
                    NativeTranscriptItem::FilesChanged {
                        native_item_id: "patch_declined".to_owned(),
                        paths: vec!["important.txt".to_owned()],
                        status: NativeEffectStatus::Declined,
                        raw: serde_json::json!({"type": "fileChange", "status": "declined"}),
                    },
                ],
            }],
        };
        let declined_native = NativeSession {
            id: ProviderSessionId::new(),
            ..native.clone()
        };
        persist_native_import(
            &store,
            &guard,
            &declined_session,
            &declined_native,
            &declined,
            false,
        )
        .unwrap();
        let declined_events = store
            .list_events(declined_session.id, 0, usize::MAX)
            .unwrap();
        assert!(
            declined_events
                .iter()
                .any(|event| event.kind == "command_declined")
        );
        assert!(
            declined_events
                .iter()
                .any(|event| event.kind == "file_change_declined")
        );
        assert!(
            !declined_events.iter().any(|event| {
                matches!(event.kind.as_str(), "command_started" | "files_changed")
            })
        );
        let declined_turn_id = deterministic_import_turn_id(
            declined_session.id,
            &declined_native.provider,
            &declined_native.native_session_id,
            "turn_declined",
        );
        assert_eq!(
            store
                .get_turn(declined_turn_id)
                .unwrap()
                .unwrap()
                .side_effect_state,
            SideEffectState::None
        );

        assert!(
            persist_native_import(&store, &guard, &populated, &native, &transcript, false).is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn codex_native_capture_appends_only_the_growing_transcript_delta() {
        let (_directory, _paths, store, populated) = fixture();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&populated.workspace_path)
                .status()
                .unwrap()
                .success()
        );
        let now = Utc::now();
        let session = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "native-capture".to_owned(),
            active_provider: Some(ProviderKind::Codex),
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            ..populated.clone()
        };
        store.create_session(&session).unwrap();
        let native = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Codex,
            native_session_id: "thr_capture".to_owned(),
            native_version: Some("1.0".to_owned()),
            capabilities: BTreeMap::from([("thread_read".to_owned(), true)]),
        };
        persist_native_attachment(&store, &session, &native, false).unwrap();
        let guard = crate::config::Config::default().payload_guard().unwrap();
        let mut transcript = NativeTranscript {
            provider: ProviderKind::Codex,
            native_session_id: native.native_session_id.clone(),
            workspace_cwd: session.workspace_path.clone(),
            turns: vec![agentctl_core::NativeTranscriptTurn {
                native_turn_id: "turn_1".to_owned(),
                status: TurnStatus::Completed,
                items: vec![
                    NativeTranscriptItem::UserPrompt {
                        native_item_id: "user_1".to_owned(),
                        text: "implement it".to_owned(),
                        attachments: Vec::new(),
                        raw: serde_json::json!({"type": "userMessage", "text": "implement it"}),
                    },
                    NativeTranscriptItem::Command {
                        native_item_id: "command_1".to_owned(),
                        command: "cargo test".to_owned(),
                        cwd: Some(session.workspace_path.display().to_string()),
                        exit_code: Some(0),
                        status: NativeEffectStatus::Completed,
                        raw: serde_json::json!({
                            "type": "commandExecution",
                            "command": "cargo test",
                            "status": "completed"
                        }),
                    },
                    NativeTranscriptItem::AssistantMessage {
                        native_item_id: "assistant_1".to_owned(),
                        text: "implemented".to_owned(),
                        final_answer: true,
                        raw: serde_json::json!({"type": "agentMessage", "text": "implemented"}),
                    },
                ],
            }],
        };

        let first = persist_native_capture(&store, &guard, &session, &native, &transcript).unwrap();
        assert_eq!(first.imported_turns, 1);
        assert_eq!(first.imported_events, 5);
        let first_events = store.list_events(session.id, 0, usize::MAX).unwrap();
        assert_eq!(first_events.len(), 6);
        assert!(first_events.iter().any(|event| {
            event.kind == "command_started"
                && event.raw_event_id.is_some()
                && event.payload.get("native_capture").is_some()
        }));
        let raw_id = first_events
            .iter()
            .find(|event| event.kind == "command_started")
            .and_then(|event| event.raw_event_id)
            .unwrap();
        let raw = store.raw_event(raw_id).unwrap().unwrap();
        assert_eq!(raw.kind, "thread/read:commandExecution");
        assert!(raw.payload.get("native_capture").is_none());
        let first_event_ids = first_events
            .iter()
            .filter(|event| event.payload.get("native_capture").is_some())
            .map(|event| event.event_id)
            .collect::<Vec<_>>();
        let stored_turn = store
            .get_turn(deterministic_import_turn_id(
                session.id,
                &native.provider,
                &native.native_session_id,
                "turn_1",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(stored_turn.side_effect_state, SideEffectState::Possible);

        transcript.turns[0].items.reverse();
        let repeated =
            persist_native_capture(&store, &guard, &session, &native, &transcript).unwrap();
        assert_eq!(repeated.imported_turns, 0);
        assert_eq!(repeated.imported_events, 0);
        assert_eq!(repeated.skipped_existing_events, 5);
        let repeated_event_ids = store
            .list_events(session.id, 0, usize::MAX)
            .unwrap()
            .into_iter()
            .filter(|event| event.payload.get("native_capture").is_some())
            .map(|event| event.event_id)
            .collect::<Vec<_>>();
        assert_eq!(first_event_ids, repeated_event_ids);

        transcript.turns.push(agentctl_core::NativeTranscriptTurn {
            native_turn_id: "turn_2".to_owned(),
            status: TurnStatus::Completed,
            items: vec![
                NativeTranscriptItem::Plan {
                    native_item_id: "plan_2".to_owned(),
                    text: "validate".to_owned(),
                    raw: serde_json::json!({"type": "plan", "text": "validate"}),
                },
                NativeTranscriptItem::FilesChanged {
                    native_item_id: "files_2".to_owned(),
                    paths: vec!["src/lib.rs".to_owned()],
                    status: NativeEffectStatus::Completed,
                    raw: serde_json::json!({
                        "type": "fileChange",
                        "paths": ["src/lib.rs"],
                        "status": "completed"
                    }),
                },
                NativeTranscriptItem::ToolCall {
                    native_item_id: "tool_2".to_owned(),
                    name: "mcp.github.get_issue".to_owned(),
                    input_summary: serde_json::json!({
                        "shape": "object",
                        "keys": ["number"],
                        "item_count": 1,
                        "size_bytes": 13,
                        "digest": "sha256:input",
                    }),
                    status: NativeEffectStatus::Completed,
                    output_digest: Some("sha256:tool-output".to_owned()),
                    artifacts: vec![agentctl_core::NativeAttachment {
                        kind: "mcp_image".to_owned(),
                        name: None,
                        media_type: Some("image/png".to_owned()),
                        source_digest: "sha256:image-output".to_owned(),
                        metadata: serde_json::json!({"source_bytes": 42}),
                        omitted: true,
                    }],
                    may_have_side_effects: true,
                    raw: serde_json::json!({
                        "type": "mcpToolCall",
                        "name": "mcp.github.get_issue",
                        "output_digest": "sha256:tool-output"
                    }),
                },
                NativeTranscriptItem::ContextMarker {
                    native_item_id: "context_2".to_owned(),
                    marker_kind: "context_compaction".to_owned(),
                    summary: "Codex compacted its native context.".to_owned(),
                    content_digest: Some("sha256:context".to_owned()),
                    raw: serde_json::json!({"type": "contextCompaction"}),
                },
                NativeTranscriptItem::AssistantMessage {
                    native_item_id: "assistant_2".to_owned(),
                    text: "validated".to_owned(),
                    final_answer: true,
                    raw: serde_json::json!({"type": "agentMessage", "text": "validated"}),
                },
            ],
        });
        let grown = persist_native_capture(&store, &guard, &session, &native, &transcript).unwrap();
        assert_eq!(grown.imported_turns, 1);
        assert_eq!(grown.imported_events, 7);
        let all_events = store.list_events(session.id, 0, usize::MAX).unwrap();
        assert!(all_events.iter().any(|event| event.kind == "tool_started"));
        assert!(all_events.iter().any(|event| {
            event.kind == "tool_completed"
                && event
                    .payload
                    .pointer("/output/digest")
                    .and_then(serde_json::Value::as_str)
                    == Some("sha256:tool-output")
        }));
        let handoff = agentctl_transcript::HandoffCapsule::from_events(
            session.id,
            &ProviderKind::Codex,
            &all_events,
        )
        .unwrap();
        let handoff_turn = handoff
            .turns
            .iter()
            .find(|turn| {
                turn.id
                    == Some(deterministic_import_turn_id(
                        session.id,
                        &native.provider,
                        &native.native_session_id,
                        "turn_2",
                    ))
            })
            .unwrap();
        assert!(
            handoff_turn
                .artifact_digests
                .iter()
                .any(|digest| digest == "sha256:tool-output")
        );
        assert!(
            handoff_turn
                .artifact_digests
                .iter()
                .any(|digest| digest == "sha256:image-output")
        );
        assert_eq!(
            handoff_turn.context_markers,
            ["Codex compacted its native context."]
        );
        let provider = store
            .provider_session(session.id, &ProviderKind::Codex)
            .unwrap()
            .unwrap();
        assert_eq!(provider.last_synced_seq, all_events.last().unwrap().seq);
        let second_turn = store
            .get_turn(deterministic_import_turn_id(
                session.id,
                &native.provider,
                &native.native_session_id,
                "turn_2",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(second_turn.side_effect_state, SideEffectState::Confirmed);

        let before_mutation = all_events.len();
        let NativeTranscriptItem::AssistantMessage { text, .. } = &mut transcript.turns[1].items[4]
        else {
            unreachable!();
        };
        *text = "changed under the same ID".to_owned();
        let error =
            persist_native_capture(&store, &guard, &session, &native, &transcript).unwrap_err();
        assert!(error.to_string().contains("changed content"));
        assert_eq!(
            store.list_events(session.id, 0, usize::MAX).unwrap().len(),
            before_mutation
        );
    }

    #[test]
    fn codex_native_capture_requires_a_linked_and_synchronized_session() {
        let (_directory, _paths, store, populated) = fixture();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(&populated.workspace_path)
                .status()
                .unwrap()
                .success()
        );
        let now = Utc::now();
        let session = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "native-capture-preconditions".to_owned(),
            parent_session_id: None,
            created_at: now,
            updated_at: now,
            ..populated.clone()
        };
        store.create_session(&session).unwrap();
        let native = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Codex,
            native_session_id: "thr_capture_preconditions".to_owned(),
            native_version: None,
            capabilities: BTreeMap::new(),
        };
        let transcript = NativeTranscript {
            provider: ProviderKind::Codex,
            native_session_id: native.native_session_id.clone(),
            workspace_cwd: session.workspace_path.clone(),
            turns: Vec::new(),
        };
        let guard = crate::config::Config::default().payload_guard().unwrap();
        assert!(
            persist_native_capture(&store, &guard, &session, &native, &transcript)
                .unwrap_err()
                .to_string()
                .contains("not linked")
        );

        persist_native_attachment(&store, &session, &native, false).unwrap();
        append_event(
            &store,
            session.id,
            store.next_seq(session.id).unwrap(),
            "assistant_final",
            serde_json::json!({"text": "not restored into Codex"}),
        );
        let error =
            persist_native_capture(&store, &guard, &session, &native, &transcript).unwrap_err();
        assert!(error.to_string().contains("sync lag"));
    }

    #[test]
    fn metrics_aggregate_canonical_provider_usage_and_sync_data() {
        let (_directory, _paths, store, session) = fixture();
        let native = NativeSession {
            id: ProviderSessionId::new(),
            provider: ProviderKind::Codex,
            native_session_id: "thread-existing".to_owned(),
            native_version: Some("1.0".to_owned()),
            capabilities: BTreeMap::new(),
        };
        persist_native_attachment(&store, &session, &native, false).unwrap();
        let usage = UsageSnapshot {
            input_tokens: Some(100),
            output_tokens: Some(25),
            cached_input_tokens: Some(40),
            cost_usd: Some(0.5),
        };
        let payload = serde_json::to_value(AgentEvent::UsageUpdated {
            usage: usage.clone(),
        })
        .unwrap();
        let turn_id = agentctl_core::TurnId::new();
        let now = Utc::now();
        store
            .create_turn(&agentctl_storage::TurnRecord {
                id: turn_id,
                session_id: session.id,
                provider: Some(ProviderKind::Codex),
                prompt_seq: 1,
                status: agentctl_core::TurnStatus::Completed,
                side_effect_state: agentctl_core::SideEffectState::None,
                native_turn_id: None,
                continuation: false,
                started_at: Some(now),
                completed_at: Some(now),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let event = CanonicalEvent {
            schema_version: 1,
            session_id: session.id,
            seq: 4,
            event_id: EventId::new(),
            turn_id: Some(turn_id),
            origin_provider: Some(ProviderKind::Codex),
            kind: "usage_updated".to_owned(),
            visibility: EventVisibility::User,
            content_hash: canonical_content_hash("usage_updated", EventVisibility::User, &payload)
                .unwrap(),
            payload,
            raw_event_id: None,
            created_at: Utc::now(),
        };
        store.append_event(&event, None).unwrap();

        let metrics = session_metrics(&store, session).unwrap();
        assert_eq!(metrics.canonical.event_count, 4);
        assert_eq!(metrics.canonical.turn_count, 1);
        assert_eq!(metrics.usage.snapshots, 1);
        assert_eq!(metrics.usage.input_tokens, 100);
        assert_eq!(metrics.usage.cached_input_tokens, 40);
        assert_eq!(metrics.providers[0].usage.output_tokens, 25);
        assert_eq!(metrics.sync.latest_seq, 4);
        assert_eq!(metrics.sync.providers_behind, 1);
        assert_eq!(metrics.sync.max_lag, 4);
    }

    #[test]
    fn ordinary_history_and_export_hide_internal_and_raw_provider_data() {
        let (directory, _paths, store, session) = fixture();
        let raw = RawProviderEvent::new(
            session.id,
            None,
            ProviderKind::Codex,
            "protocol-frame",
            serde_json::json!({"secret": "raw-only-value"}),
        )
        .unwrap();
        let payload = serde_json::json!({
            "raw_event": {
                "event_id": raw.event_id,
                "content_hash": raw.content_hash,
            }
        });
        let internal = CanonicalEvent {
            schema_version: 1,
            session_id: session.id,
            seq: 3,
            event_id: EventId::new(),
            turn_id: None,
            origin_provider: Some(ProviderKind::Codex),
            kind: "provider_raw".to_owned(),
            visibility: EventVisibility::Internal,
            content_hash: canonical_content_hash(
                "provider_raw",
                EventVisibility::Internal,
                &payload,
            )
            .unwrap(),
            payload,
            raw_event_id: Some(raw.event_id),
            created_at: Utc::now(),
        };
        store.append_event(&internal, Some(&raw)).unwrap();

        let ordinary = session_history(&store, session.id, false, None).unwrap();
        assert_eq!(ordinary.len(), 2);
        assert!(
            ordinary
                .iter()
                .all(|entry| entry.event.visibility != EventVisibility::Internal)
        );
        let diagnostic = session_history(&store, session.id, true, None).unwrap();
        assert_eq!(diagnostic.len(), 3);
        assert!(diagnostic.last().unwrap().raw.is_some());

        let output = directory.path().join("ordinary-export.jsonl");
        let report = export_session(
            &store,
            session,
            &output,
            ExportOptions {
                include_blobs: false,
                redact: false,
                include_internal: false,
            },
        )
        .unwrap();
        assert_eq!(report.event_count, 2);
        let serialized = fs::read_to_string(output).unwrap();
        assert!(!serialized.contains("raw-only-value"));
        assert!(!serialized.contains("provider_raw"));
    }

    #[test]
    fn retention_deletes_stale_leaves_but_preserves_referenced_parents() {
        let (_directory, _paths, store, session) = fixture();
        let old = Utc::now() - chrono::TimeDelta::days(90);
        let parent = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "stale-parent".to_owned(),
            workspace_path: session.workspace_path.clone(),
            workspace_fingerprint: session.workspace_fingerprint.clone(),
            active_provider: None,
            routing_policy: session.routing_policy.clone(),
            auth_mode: session.auth_mode,
            status: SessionStatus::Active,
            parent_session_id: None,
            created_at: old,
            updated_at: old,
            schema_version: 1,
        };
        store.create_session(&parent).unwrap();
        let child = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "recent-child".to_owned(),
            parent_session_id: Some(parent.id),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ..parent.clone()
        };
        store.create_session(&child).unwrap();
        let stale_leaf = UnifiedSession {
            id: UnifiedSessionId::new(),
            name: "stale-leaf".to_owned(),
            parent_session_id: None,
            ..parent.clone()
        };
        store.create_session(&stale_leaf).unwrap();
        let stale_blob = store.blobs.put(b"expired", "text/plain", true).unwrap();
        store.register_blob(&stale_blob, old).unwrap();

        let report = enforce_retention(&store, 30, Utc::now()).unwrap();
        assert_eq!(report.deleted_sessions, vec![stale_leaf.id]);
        assert_eq!(report.deleted_blobs, vec![stale_blob.digest.clone()]);
        assert_eq!(report.retained_due_to_descendants, vec![parent.id]);
        assert!(store.get_session(parent.id).unwrap().is_some());
        assert!(store.get_session(child.id).unwrap().is_some());
        assert!(store.get_session(stale_leaf.id).unwrap().is_none());
        assert!(!store.blobs.contains(&stale_blob.digest).unwrap());
    }

    #[test]
    fn retention_preserves_every_open_native_launch_state() {
        let (directory, _paths, store, template) = fixture();
        let old = Utc::now() - chrono::TimeDelta::days(90);
        let mut expected = Vec::new();
        for (index, state) in [
            NativeLaunchState::Started,
            NativeLaunchState::CaptureReady,
            NativeLaunchState::Exited,
            NativeLaunchState::Uncertain,
        ]
        .into_iter()
        .enumerate()
        {
            let workspace = directory.path().join(format!("retained-{index}"));
            fs::create_dir_all(&workspace).unwrap();
            let retained = UnifiedSession {
                id: UnifiedSessionId::new(),
                name: format!("retained-{state:?}"),
                workspace_path: workspace,
                workspace_fingerprint: format!("sha256:retained-{index}"),
                active_provider: None,
                parent_session_id: None,
                created_at: old,
                updated_at: old,
                ..template.clone()
            };
            store.create_session(&retained).unwrap();
            journal_native_launch(&store, &retained, state);
            expected.push(retained.id);
        }
        expected.sort();

        let report = enforce_retention(&store, 30, Utc::now()).unwrap();
        assert_eq!(report.retained_due_to_native_launch, expected);
        for id in expected {
            assert!(store.get_session(id).unwrap().is_some());
            assert!(!report.deleted_sessions.contains(&id));
        }
    }

    #[test]
    fn export_import_transfers_verified_blob_companion() {
        let (directory, _paths, store, session) = fixture();
        let blob = store
            .blobs
            .put(b"blob payload", "text/plain", true)
            .unwrap();
        store.register_blob(&blob, Utc::now()).unwrap();
        append_event(
            &store,
            session.id,
            3,
            "tool_completed",
            serde_json::json!({"output": blob}),
        );
        let output = directory.path().join("with-blobs.jsonl");
        let exported = export_session(
            &store,
            session,
            &output,
            ExportOptions {
                include_blobs: true,
                redact: false,
                include_internal: false,
            },
        )
        .unwrap();
        assert_eq!(exported.included_blobs, 1);
        assert!(exported.blob_directory.unwrap().is_dir());

        let imported_paths =
            AgentctlPaths::resolve(Some(directory.path().join("blob-import-home"))).unwrap();
        let imported_store = open_store(&imported_paths).unwrap();
        let imported = import_session(&imported_store, &output).unwrap();
        assert_eq!(imported.imported_blobs, 1);
    }

    #[test]
    fn redacted_export_import_compaction_and_fork_are_real_store_operations() {
        let (directory, _paths, store, session) = fixture();
        let output = directory.path().join("export.jsonl");
        let export = export_session(
            &store,
            session.clone(),
            &output,
            ExportOptions {
                include_blobs: false,
                redact: true,
                include_internal: false,
            },
        )
        .unwrap();
        assert_eq!(export.event_count, 2);
        assert!(
            !fs::read_to_string(&output)
                .unwrap()
                .contains("secret-value")
        );

        let imported_paths =
            AgentctlPaths::resolve(Some(directory.path().join("imported-home"))).unwrap();
        let imported_store = open_store(&imported_paths).unwrap();
        let imported = import_session(&imported_store, &output).unwrap();
        assert_eq!(imported.event_count, 2);
        assert_eq!(imported.session.active_provider, None);

        let compacted = compact_session(&store, session.id, CompactionPolicy::default()).unwrap();
        assert_eq!(compacted.event_seq, 3);
        assert_eq!(
            store
                .latest_checkpoint(session.id)
                .unwrap()
                .unwrap()
                .projection_version,
            1
        );
        let forked = fork_session(&store, &session, Some("child"), Some(2)).unwrap();
        assert_eq!(forked.copied_events, 2);
        assert_eq!(
            store.list_events(forked.session.id, 0, 10).unwrap().len(),
            2
        );
    }

    #[test]
    fn plugin_installation_canonicalizes_and_validates_executable() {
        let (directory, paths, _store, _session) = fixture();
        let source = directory.path().join("plugin-source");
        fs::create_dir_all(&source).unwrap();
        let executable = source.join("provider");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let manifest = PluginManifest {
            manifest_version: 1,
            name: "example".into(),
            version: "1.0.0".into(),
            protocol_versions: vec![CURRENT_PROTOCOL_VERSION],
            executable: PathBuf::from("provider"),
            args: Vec::new(),
            env: BTreeMap::new(),
            capabilities: PluginCapabilities::default(),
        };
        let manifest_path = source.join("plugin.toml");
        fs::write(&manifest_path, manifest.to_toml().unwrap()).unwrap();

        let installed = install_plugin(&paths, &manifest_path).unwrap();
        assert!(installed.plugin.manifest.executable.is_absolute());
        assert_eq!(list_plugins(&paths).unwrap().len(), 1);
        assert!(matches!(
            doctor_plugins(&paths, Some("example")).unwrap()[0].state,
            PluginDoctorState::Compatible
        ));
        assert!(remove_plugin(&paths, "example").unwrap().removed);
    }

    #[test]
    fn repair_checks_integrity_and_blob_digests() {
        let (_directory, paths, store, _session) = fixture();
        let blob = store.blobs.put(b"verified", "text/plain", true).unwrap();
        store.register_blob(&blob, Utc::now()).unwrap();
        let report = repair_local_state(&paths, &store, true, None).unwrap();
        assert!(report.healthy());
        assert_eq!(report.blob_files_checked, 1);
    }

    #[test]
    fn projection_rebuild_refuses_an_open_native_launch() {
        let (_directory, paths, store, session) = fixture();
        let launch_id = journal_native_launch(&store, &session, NativeLaunchState::Uncertain);
        let error = repair_local_state(&paths, &store, true, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("projection rebuild refused"));
        assert!(error.contains(&launch_id.to_string()));

        // Integrity-only repair does not rewrite provider projection state.
        let report = repair_local_state(&paths, &store, false, None).unwrap();
        assert!(!report.healthy());
        assert_eq!(report.unresolved_native_launches[0].id, launch_id);
    }
}
