#![cfg(unix)]

use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::Path};

use agentctl_core::{
    AgentProvider, AuthMode, NativeSession, ProviderKind, ProviderSessionId, SessionContext,
    UnifiedSessionId,
};
use agentctl_provider_codex::{CodexAdapter, CodexConfig};
use serde_json::{Value, json};

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn install_schema(directory: &Path) {
    std::fs::write(
        directory.join("ClientRequest.json"),
        serde_json::to_vec(&json!({
            "methods": ["thread/start", "thread/resume", "thread/inject_items", "thread/read"]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn install_fake(binary: &Path, body: &str) {
    std::fs::write(
        binary,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf '%s\\n' 'codex-cli protocol-test'\n  exit 0\nfi\n{body}\n"
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(binary).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(binary, permissions).unwrap();
}

fn context(workspace: &Path, id: UnifiedSessionId) -> SessionContext {
    SessionContext {
        unified_session_id: id,
        workspace_root: workspace.to_path_buf(),
        workspace_fingerprint: "materialization-protocol-test".to_owned(),
        auth_mode: AuthMode::NativeLocal,
    }
}

#[tokio::test]
async fn new_thread_is_materialized_and_verified_exactly_once() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let schema_root = directory.path().join("schema");
    let schema = schema_root.join("codex-cli_protocol-test");
    std::fs::create_dir_all(&schema).unwrap();
    install_schema(&schema);
    let trace = directory.path().join("trace.jsonl");
    let binary = directory.path().join("fake-codex");
    let cwd = serde_json::to_string(workspace.path().to_str().unwrap()).unwrap();
    install_fake(
        &binary,
        &format!(
            r#"IFS= read -r initialize
printf '%s\n' '{{"id":1,"result":{{"capabilities":{{}}}}}}'
IFS= read -r initialized
IFS= read -r start
printf '%s\n' "$start" >> {trace}
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thr_materialized"}}}}}}'
IFS= read -r inject
printf '%s\n' "$inject" >> {trace}
printf '%s\n' '{{"id":3,"result":{{}}}}'
IFS= read -r read_thread
printf '%s\n' "$read_thread" >> {trace}
printf '%s\n' '{{"id":4,"result":{{"thread":{{"id":"thr_materialized","cwd":{cwd},"turns":[]}}}}}}'
sleep 30
"#,
            trace = shell_quote(&trace),
        ),
    );

    let adapter = CodexAdapter::from_config(CodexConfig {
        binary,
        schema_cache_root: Some(schema_root),
        ..CodexConfig::default()
    });
    let unified_session_id = UnifiedSessionId::new();
    let context = context(workspace.path(), unified_session_id);
    let created = adapter.ensure_session(&context).await.unwrap();
    let ensured_again = adapter.ensure_session(&context).await.unwrap();
    assert_eq!(created.native_session_id, "thr_materialized");
    assert_eq!(ensured_again.native_session_id, created.native_session_id);
    adapter.shutdown().await.unwrap();

    let requests = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3, "a second ensure must not inject again");
    assert_eq!(requests[0]["method"], "thread/start");
    assert_eq!(requests[1]["method"], "thread/inject_items");
    assert_eq!(requests[2]["method"], "thread/read");
    assert_eq!(
        requests[1]["params"]["items"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(requests[1]["params"]["items"][0]["role"], "assistant");
    assert_eq!(
        requests[1]["params"]["items"][0]["content"][0]["type"],
        "output_text"
    );
    assert!(
        requests[1]["params"]["items"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("purpose=\"rollout-materialization\"")
    );
    assert_eq!(requests[2]["params"]["includeTurns"], true);
}

#[tokio::test]
async fn restoring_an_existing_thread_never_injects_the_materialization_marker() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let trace = directory.path().join("trace.jsonl");
    let binary = directory.path().join("fake-codex");
    install_fake(
        &binary,
        &format!(
            r#"IFS= read -r initialize
printf '%s\n' '{{"id":1,"result":{{"capabilities":{{}}}}}}'
IFS= read -r initialized
IFS= read -r resume
printf '%s\n' "$resume" >> {trace}
printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thr_existing"}}}}}}'
sleep 30
"#,
            trace = shell_quote(&trace),
        ),
    );

    let adapter = CodexAdapter::from_config(CodexConfig {
        binary,
        ..CodexConfig::default()
    });
    let unified_session_id = UnifiedSessionId::new();
    let persisted = NativeSession {
        id: ProviderSessionId::new(),
        provider: ProviderKind::Codex,
        native_session_id: "thr_existing".to_owned(),
        native_version: Some("codex-cli protocol-test".to_owned()),
        capabilities: BTreeMap::default(),
    };
    let restored = adapter
        .restore_session(&context(workspace.path(), unified_session_id), persisted)
        .await
        .unwrap();
    assert_eq!(restored.native_session_id, "thr_existing");
    adapter.shutdown().await.unwrap();

    let requests = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["method"], "thread/resume");
}
