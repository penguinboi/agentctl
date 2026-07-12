//! Native Claude Code stream-json adapter.

mod adapter;
mod capabilities;
mod hooks;
mod mapping;
pub mod native_hooks;
mod process;
mod protocol;

pub use adapter::{
    ClaudeAdapter, ClaudeConfig, ShouldQueryProbeReport, ShouldQuerySupport,
    UserPromptSubmitHookProbeReport, UserPromptSubmitHookSupport,
};
pub use capabilities::{
    ClaudeAuthStatus, ClaudeInstallation, detect_auth_status, detect_installation,
};
pub use hooks::HANDOFF_POLICY;
