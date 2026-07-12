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
