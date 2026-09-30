// ABOUTME: Exercises the installed Codex app-server against disposable native sessions.
// ABOUTME: Verifies protocol behavior without submitting conversational model turns.
use agentctl_core::{
    AgentProvider, AuthMode, CanonicalEvent, EventId, EventVisibility, ProviderStatus,
    SessionContext, SyncBatch, UnifiedSessionId,
};
use agentctl_provider_codex::CodexAdapter;
use chrono::Utc;
use serde_json::json;

#[tokio::test]
#[ignore = "requires a locally installed and authenticated Codex CLI"]
async fn installed_app_server_handshake_and_account_probe() {
    let adapter = CodexAdapter::new("codex", None);
    let health = adapter.probe_live().await.unwrap();
    assert!(matches!(
        health.status,
        ProviderStatus::Ready
            | ProviderStatus::Warning
            | ProviderStatus::Exhausted { .. }
            | ProviderStatus::AuthError
    ));
    adapter.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a locally installed Codex CLI"]
async fn installed_empty_thread_is_materialized_for_restart_resume_without_turn() {
    let workspace = tempfile::TempDir::new().unwrap();
    let unified_session_id = UnifiedSessionId::new();
    let context = SessionContext {
        unified_session_id,
        workspace_root: workspace.path().to_path_buf(),
        workspace_fingerprint: "empty-thread-live-probe".to_owned(),
        auth_mode: AuthMode::NativeLocal,
    };
    let adapter = CodexAdapter::new("codex", None);
    let session = adapter.ensure_session(&context).await.unwrap();
    adapter.shutdown().await.unwrap();

    let restarted = CodexAdapter::new("codex", None);
    let restored = restarted.restore_session(&context, session).await.unwrap();
    let transcript = restarted.read_native_history(&restored).await.unwrap();
    assert!(
        transcript.turns.is_empty(),
        "materializing an empty thread must not create a user-authored native turn"
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a locally installed and authenticated Codex CLI"]
async fn installed_thread_injection_and_restart_resume() {
    let workspace = tempfile::TempDir::new().unwrap();
    let unified_session_id = UnifiedSessionId::new();
    let context = SessionContext {
        unified_session_id,
        workspace_root: workspace.path().to_path_buf(),
        workspace_fingerprint: "live-probe".to_owned(),
        auth_mode: AuthMode::NativeLocal,
    };
    let adapter = CodexAdapter::new("codex", None);
    let session = adapter.ensure_session(&context).await.unwrap();
    let receipt = adapter
        .sync_context(
            &session,
            SyncBatch {
                from_seq_exclusive: 0,
                through_seq_inclusive: 1,
                projection_version: 1,
                events: vec![CanonicalEvent {
                    schema_version: 1,
                    session_id: unified_session_id,
                    seq: 1,
                    event_id: EventId::new(),
                    turn_id: None,
                    origin_provider: None,
                    kind: "assistant_final".to_owned(),
                    visibility: EventVisibility::Projection,
                    payload: json!({"text": "agentctl live probe context"}),
                    content_hash: "sha256:live-probe".to_owned(),
                    raw_event_id: None,
                    created_at: Utc::now(),
                }],
                handoff: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt.through_seq, 1);
    adapter.shutdown().await.unwrap();

    let restarted = CodexAdapter::new("codex", None);
    let restored = restarted.restore_session(&context, session).await.unwrap();
    assert!(!restored.native_session_id.is_empty());
    let transcript = restarted.read_native_history(&restored).await.unwrap();
    assert!(
        transcript.turns.is_empty(),
        "thread/inject_items must not be recaptured as a user-authored native turn"
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a locally installed Codex CLI"]
async fn installed_native_history_can_be_read_while_another_client_owns_the_thread() {
    let workspace = tempfile::TempDir::new().unwrap();
    let context = SessionContext {
        unified_session_id: UnifiedSessionId::new(),
        workspace_root: workspace.path().to_path_buf(),
        workspace_fingerprint: "history-reader-live-probe".to_owned(),
        auth_mode: AuthMode::NativeLocal,
    };
    let owner = CodexAdapter::new("codex", None);
    let session = owner.ensure_session(&context).await.unwrap();
    let reader = CodexAdapter::new("codex", None);
    let transcript = reader.read_native_history(&session).await;
    reader.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();

    let transcript = transcript.unwrap();
    assert_eq!(transcript.native_session_id, session.native_session_id);
    assert_eq!(
        transcript.workspace_cwd,
        workspace.path().canonicalize().unwrap()
    );
    assert!(transcript.turns.is_empty());
}
