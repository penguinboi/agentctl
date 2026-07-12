use std::{ffi::OsString, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "agentctl", version, about, long_about = None)]
pub struct Cli {
    /// Override the agentctl state directory.
    #[arg(long, global = true, env = "AGENTCTL_HOME")]
    pub home: Option<PathBuf>,

    /// Emit structured JSON where supported.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open a canonical session in the provider's native interactive CLI.
    Open(OpenArgs),
    /// Synchronize the canonical delta and switch to another native interactive CLI.
    Switch(SwitchArgs),
    /// Create a canonical session and open the selected native CLI unless --no-launch.
    New(NewArgs),
    /// Reopen a canonical session in its native CLI.
    Resume(ResumeArgs),
    /// List canonical sessions.
    List,
    /// Show session, provider, quota, and workspace status.
    Status(OptionalSessionArg),
    /// Show local canonical, provider, usage, and synchronization aggregates.
    Metrics(OptionalSessionArg),
    /// Check provider and local-state compatibility.
    Doctor(DoctorArgs),
    /// Show canonical history.
    History(HistoryArgs),
    /// Export a session and optional blobs.
    Export(ExportArgs),
    /// Import a canonical export.
    Import(ImportArgs),
    /// Attach an existing provider-native session without importing its transcript.
    Attach(AttachArgs),
    /// Import an existing Codex transcript through its official history API.
    ImportNative(ImportNativeArgs),
    /// Delete a local canonical session.
    Delete(SessionArg),
    /// Repair permissions, indexes, blobs, receipts, and projections.
    Repair(RepairArgs),
    /// Compact a session projection into a deterministic checkpoint.
    Compact(SessionArg),
    /// Synchronize lagging provider projections.
    Sync(OptionalSessionArg),
    /// Fork a canonical session.
    Fork(ForkArgs),
    /// Select the routing provider or policy.
    Provider(ProviderArgs),
    /// Manage known workspaces.
    Workspace(WorkspaceArgs),
    /// Manage process-isolated provider plugins.
    Plugin(PluginArgs),
    /// Internal entrypoint used by provider-native lifecycle hooks.
    #[command(hide = true)]
    Hook(HookArgs),
}

#[derive(Debug, Args)]
pub struct OpenArgs {
    /// Native provider to open. Defaults to the session's active provider.
    #[arg(value_enum)]
    pub provider: Option<NativeProviderChoice>,
    /// Canonical session UUID or unique name. Defaults to the current workspace session.
    #[arg(long)]
    pub session: Option<String>,
    /// Allowlisted native CLI options. Positional prompts are rejected.
    #[arg(last = true, allow_hyphen_values = true)]
    pub native_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct SwitchArgs {
    #[arg(value_enum)]
    pub provider: NativeProviderChoice,
    /// Canonical session UUID or unique name. Defaults to the current workspace session.
    #[arg(long)]
    pub session: Option<String>,
    /// Allowlisted native CLI options. Positional prompts are rejected.
    #[arg(last = true, allow_hyphen_values = true)]
    pub native_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct NewArgs {
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,
    #[arg(long, value_enum, default_value_t = ProviderChoice::Auto)]
    pub provider: ProviderChoice,
    /// Create the canonical session without opening a native provider CLI.
    #[arg(long)]
    pub no_launch: bool,
    /// Allowlisted native CLI options for the initial native launch. Positional
    /// prompts are rejected. Cannot be combined with `--no-launch`.
    #[arg(last = true, allow_hyphen_values = true, conflicts_with = "no_launch")]
    pub native_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    pub session: String,
    /// Override the session's active provider for this launch.
    #[arg(long, value_enum)]
    pub provider: Option<NativeProviderChoice>,
    /// Allowlisted native CLI options. Positional prompts are rejected.
    #[arg(last = true, allow_hyphen_values = true)]
    pub native_args: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum ProviderChoice {
    #[default]
    Auto,
    Claude,
    Codex,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NativeProviderChoice {
    Claude,
    Codex,
}

#[derive(Debug, Args)]
pub struct SessionArg {
    pub session: String,
}

#[derive(Debug, Args)]
pub struct OptionalSessionArg {
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Execute disposable real turns; this may consume provider quota.
    #[arg(long)]
    pub live: bool,
    /// Rewrite the compatibility report at this path.
    #[arg(long)]
    pub report: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    pub session: Option<String>,
    #[arg(long)]
    pub raw: bool,
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    pub session: String,
    pub output: PathBuf,
    #[arg(long)]
    pub include_blobs: bool,
    #[arg(long)]
    pub redact: bool,
    /// Include internal normalized diagnostics. Raw provider frames are never portable-exported.
    #[arg(long)]
    pub include_internal: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    pub input: PathBuf,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    #[arg(value_enum)]
    pub provider: NativeProviderChoice,
    /// Existing Codex thread id or Claude session UUID.
    pub native_session_id: String,
    /// Canonical session UUID or unique name. Defaults to the current workspace session.
    #[arg(long)]
    pub session: Option<String>,
    /// Make the attached provider the active manual provider.
    #[arg(long)]
    pub activate: bool,
}

#[derive(Debug, Args)]
pub struct ImportNativeArgs {
    /// Provider whose official history API will be used. Only Codex currently exposes one.
    #[arg(value_enum)]
    pub provider: ImportNativeProviderChoice,
    /// Existing Codex thread id.
    pub native_session_id: String,
    /// Canonical session UUID or unique name. Defaults to the current workspace session.
    #[arg(long)]
    pub session: Option<String>,
    /// Make Codex the active manual provider.
    #[arg(long)]
    pub activate: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ImportNativeProviderChoice {
    Codex,
}

#[derive(Debug, Args)]
pub struct RepairArgs {
    #[arg(long)]
    pub rebuild_projections: bool,
    /// Abandon an exited/uncertain launch after verifying its provider process
    /// is dead. Requires --rebuild-projections; launches without a PID stay blocked.
    #[arg(long, value_name = "LAUNCH_ID")]
    pub abandon_native_launch: Option<uuid::Uuid>,
}

#[derive(Debug, Args)]
pub struct ForkArgs {
    pub session: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub through_seq: Option<u64>,
}

#[derive(Debug, Args)]
pub struct ProviderArgs {
    #[arg(value_enum)]
    pub provider: ProviderChoice,
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct WorkspaceArgs {
    #[command(subcommand)]
    pub command: WorkspaceCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    List,
    Use { path: PathBuf },
}

#[derive(Debug, Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(Debug, Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub command: HookCommand,
}

#[derive(Debug, Subcommand)]
pub enum HookCommand {
    /// Capture one Claude Code native hook event.
    Claude(ClaudeHookArgs),
}

#[derive(Debug, Args)]
pub struct ClaudeHookArgs {
    /// Canonical agentctl session UUID.
    #[arg(long)]
    pub session: String,
    /// Claude Code session UUID assigned to this canonical projection.
    #[arg(long)]
    pub expected_native_session: String,
    /// Journaled native launch which authorized this hook invocation.
    #[arg(long)]
    pub launch_id: String,
    /// Legacy path argument, rejected by the native bridge when supplied.
    #[arg(long)]
    pub handoff: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    List,
    Install { manifest: PathBuf },
    Remove { name: String },
    Doctor { name: Option<String> },
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use clap::error::ErrorKind;

    use super::{Cli, Command};

    #[test]
    fn new_forwards_native_options_to_its_initial_native_cli() {
        let cli = Cli::try_parse_from([
            "agentctl",
            "new",
            "--name",
            "native",
            "--provider",
            "claude",
            "--",
            "--model",
            "sonnet",
        ])
        .unwrap();
        let Some(Command::New(args)) = cli.command else {
            panic!("expected new command");
        };
        assert_eq!(
            args.native_args,
            [
                std::ffi::OsString::from("--model"),
                std::ffi::OsString::from("sonnet")
            ]
        );
    }

    #[test]
    fn new_rejects_native_options_when_no_native_cli_will_launch() {
        let error =
            Cli::try_parse_from(["agentctl", "new", "--no-launch", "--", "--model", "sonnet"])
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn native_history_import_is_codex_only_at_the_cli_boundary() {
        let error =
            Cli::try_parse_from(["agentctl", "import-native", "claude", "session-id"]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);

        let parsed =
            Cli::try_parse_from(["agentctl", "import-native", "codex", "thread-id"]).unwrap();
        assert!(matches!(parsed.command, Some(Command::ImportNative(_))));
    }
}
