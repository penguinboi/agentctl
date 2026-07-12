//! Native Codex app-server adapter.

mod adapter;
mod jsonrpc;
mod mapping;
mod process;
mod schema;

pub use adapter::{
    CodexAdapter, CodexConfig, InteractiveThreadIdentity, InteractiveThreadSnapshot,
    MAX_INTERACTIVE_THREAD_SNAPSHOT,
};
pub use jsonrpc::{RpcInbound, RpcResponse};
pub use mapping::{map_notification, parse_rate_limit};
pub use schema::{CodexInstallation, detect_installation, generate_schema_cache};
