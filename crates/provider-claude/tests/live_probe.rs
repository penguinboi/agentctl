use agentctl_core::{AgentProvider, ProviderStatus};
use agentctl_provider_claude::ClaudeAdapter;

#[tokio::test]
#[ignore = "requires a locally installed Claude Code CLI; creates no model turn"]
async fn installed_stream_json_control_handshake() {
    let adapter = ClaudeAdapter::new("claude", None, None);
    let health = adapter
        .probe_live(&std::env::current_dir().unwrap())
        .await
        .unwrap();
    assert!(matches!(health.status, ProviderStatus::Ready));
    adapter.shutdown().await.unwrap();
}
