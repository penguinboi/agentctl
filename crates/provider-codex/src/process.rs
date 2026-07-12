use std::{path::Path, sync::Arc, time::Duration};

use agentctl_core::ProviderError;

use crate::jsonrpc::CodexRpcClient;

pub(crate) async fn spawn_app_server(
    binary: &Path,
    channel_capacity: usize,
    request_timeout: Duration,
) -> Result<Arc<CodexRpcClient>, ProviderError> {
    CodexRpcClient::spawn(binary, channel_capacity, request_timeout).await
}
