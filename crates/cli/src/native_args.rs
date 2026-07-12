//! Fail-closed validation for arguments forwarded to provider-native CLIs.
//!
//! Agentctl owns the native session identity, workspace, lifecycle, and the
//! configuration required to capture and project a conversation. Forwarded
//! arguments are therefore an explicit, versioned allowlist rather than an
//! arbitrary provider command line. In particular, this module never accepts a
//! positional argument: a positional value which is not consumed by a known
//! option would be an initial prompt (or a provider subcommand).

use std::ffi::OsString;

use agentctl_core::ProviderKind;
use anyhow::{Result, bail};

#[derive(Clone, Copy)]
enum ValuePolicy {
    Any,
    OneOf(&'static [&'static str]),
    OptionalAttached(&'static [&'static str]),
}

#[derive(Clone, Copy)]
enum OptionArity {
    Flag,
    Value(ValuePolicy),
}

#[derive(Clone, Copy)]
struct AllowedOption {
    long: &'static str,
    short: Option<&'static str>,
    arity: OptionArity,
}

const CLAUDE_OPTIONS: &[AllowedOption] = &[
    value("--model", None, ValuePolicy::Any),
    value(
        "--effort",
        None,
        ValuePolicy::OneOf(&["low", "medium", "high", "xhigh", "max"]),
    ),
    // `bypassPermissions` intentionally remains unavailable. Users still get
    // the provider-native approval UI without letting a forwarded argument
    // silently disable it.
    value(
        "--permission-mode",
        None,
        ValuePolicy::OneOf(&["acceptEdits", "auto", "manual", "dontAsk", "plan"]),
    ),
    value("--debug", Some("-d"), ValuePolicy::OptionalAttached(&[])),
    value(
        "--prompt-suggestions",
        None,
        ValuePolicy::OptionalAttached(&["true", "false", "1", "0", "yes", "no", "on", "off"]),
    ),
    flag("--verbose"),
    flag("--chrome"),
    flag("--no-chrome"),
    flag("--ide"),
    flag("--ax-screen-reader"),
    flag("--brief"),
    flag("--disable-slash-commands"),
    flag("--exclude-dynamic-system-prompt-sections"),
];

const CODEX_OPTIONS: &[AllowedOption] = &[
    value("--model", Some("-m"), ValuePolicy::Any),
    value(
        "--sandbox",
        Some("-s"),
        ValuePolicy::OneOf(&["read-only", "workspace-write", "danger-full-access"]),
    ),
    value(
        "--ask-for-approval",
        Some("-a"),
        ValuePolicy::OneOf(&["untrusted", "on-request", "never"]),
    ),
    value(
        "--local-provider",
        None,
        ValuePolicy::OneOf(&["lmstudio", "ollama"]),
    ),
    flag("--strict-config"),
    flag("--oss"),
    flag("--search"),
    flag("--no-alt-screen"),
];

const CLAUDE_OWNED_OPTIONS: &[&str] = &[
    "--session-id",
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--fork-session",
    "--settings",
    "--setting-sources",
    "--system-prompt",
    "--system-prompt-file",
    "--append-system-prompt",
    "--append-system-prompt-file",
    "--agent",
    "--agents",
    "--worktree",
    "-w",
    "--add-dir",
    "--print",
    "-p",
    "--input-format",
    "--output-format",
    "--json-schema",
    "--include-hook-events",
    "--include-partial-messages",
    "--replay-user-messages",
    "--fallback-model",
    "--max-budget-usd",
    "--no-session-persistence",
    "--bare",
    "--safe-mode",
    "--background",
    "--bg",
    "--from-pr",
    "--remote-control",
    "--remote-control-session-name-prefix",
    "--tmux",
    "--mcp-config",
    "--strict-mcp-config",
    "--plugin-dir",
    "--plugin-url",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--tools",
    "--file",
    "--betas",
    "--debug-file",
    "--name",
    "-n",
    "--allow-dangerously-skip-permissions",
    "--dangerously-skip-permissions",
    "--help",
    "-h",
    "--version",
    "-v",
];

const CODEX_OWNED_OPTIONS: &[&str] = &[
    "--config",
    "-c",
    "--enable",
    "--disable",
    "--profile",
    "-p",
    "--remote",
    "--remote-auth-token-env",
    "--cd",
    "-C",
    "--add-dir",
    "--image",
    "-i",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "--background",
    "--bg",
    "--detach",
    "--daemon",
    "--last",
    "--all",
    "--include-non-interactive",
    "--help",
    "-h",
    "--version",
    "-V",
];

const fn flag(long: &'static str) -> AllowedOption {
    AllowedOption {
        long,
        short: None,
        arity: OptionArity::Flag,
    }
}

const fn value(
    long: &'static str,
    short: Option<&'static str>,
    policy: ValuePolicy,
) -> AllowedOption {
    AllowedOption {
        long,
        short,
        arity: OptionArity::Value(policy),
    }
}

/// Validates arguments which will be appended to a provider-native interactive
/// invocation.
///
/// Only explicitly allowed options are accepted. Option values must be UTF-8,
/// non-empty, and cannot begin with `-`; that conservative rule prevents a
/// misspelled or missing value from consuming the following flag. Optional
/// values are accepted only in attached `--option=value` form so the next
/// positional token can never be mistaken for an option value.
pub(crate) fn validate(provider: &ProviderKind, args: &[OsString]) -> Result<()> {
    let (allowed, owned) = match provider {
        ProviderKind::Claude => (CLAUDE_OPTIONS, CLAUDE_OWNED_OPTIONS),
        ProviderKind::Codex => (CODEX_OPTIONS, CODEX_OWNED_OPTIONS),
        ProviderKind::Plugin(_) => bail!("plugins do not expose a native interactive CLI"),
    };

    let mut index = 0;
    while index < args.len() {
        let argument = utf8_argument(provider, &args[index])?;
        if argument == "--" {
            bail!(
                "native {provider} argument delimiter `--` is not allowed because positional prompts are owned by the native interactive UI"
            );
        }
        if !argument.starts_with('-') {
            bail!(
                "native {provider} positional argument `{argument}` is not allowed; enter prompts in the native interactive UI"
            );
        }

        let (name, attached) = split_option(argument);
        let Some(option) = allowed
            .iter()
            .find(|option| option.long == name || option.short == Some(name))
        else {
            if owned.contains(&name) {
                bail!(
                    "native {provider} argument `{name}` conflicts with agentctl session, workspace, settings, security, or lifecycle ownership"
                );
            }
            bail!(
                "native {provider} argument `{name}` is not in agentctl's safe forwarding allowlist"
            );
        };

        match option.arity {
            OptionArity::Flag => {
                if attached.is_some() {
                    bail!("native {provider} flag `{name}` does not accept a value");
                }
            }
            OptionArity::Value(ValuePolicy::OptionalAttached(accepted)) => {
                if let Some(value) = attached {
                    validate_value(provider, name, value, accepted)?;
                }
                // Never consume the following token for an optional value. A
                // provider may support that spelling, but it is ambiguous with
                // a positional prompt at this trust boundary.
            }
            OptionArity::Value(policy) => {
                let (value, consumed_next) = if let Some(value) = attached {
                    (value, false)
                } else {
                    let value = args.get(index + 1).ok_or_else(|| {
                        anyhow::anyhow!(
                            "native {provider} option `{name}` requires an explicit value"
                        )
                    })?;
                    (utf8_argument(provider, value)?, true)
                };
                let accepted = match policy {
                    ValuePolicy::Any => &[][..],
                    ValuePolicy::OneOf(accepted) => accepted,
                    ValuePolicy::OptionalAttached(_) => unreachable!(),
                };
                validate_value(provider, name, value, accepted)?;
                if consumed_next {
                    index += 1;
                }
            }
        }

        index += 1;
    }
    Ok(())
}

/// Validates forwarded arguments before any provider selection or durable
/// state transition can occur.
///
/// When the provider was not selected explicitly, every argument must be safe
/// for both native CLIs. Otherwise provider selection itself could turn an
/// apparently valid invocation into a provider-specific prompt or option after
/// health probing and routing have already mutated local state.
pub(crate) fn preflight(provider: Option<&ProviderKind>, args: &[OsString]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    if let Some(provider) = provider {
        return validate(provider, args);
    }

    let claude = validate(&ProviderKind::Claude, args);
    let codex = validate(&ProviderKind::Codex, args);
    match (claude, codex) {
        (Ok(()), Ok(())) => Ok(()),
        (claude, codex) => {
            let detail = [
                claude.err().map(|error| format!("Claude: {error}")),
                codex.err().map(|error| format!("Codex: {error}")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
            bail!(
                "native arguments with automatic provider selection must be valid for both Claude and Codex; select a provider explicitly ({detail})"
            )
        }
    }
}

fn utf8_argument<'a>(provider: &ProviderKind, argument: &'a OsString) -> Result<&'a str> {
    argument.to_str().ok_or_else(|| {
        anyhow::anyhow!("native {provider} arguments must be valid UTF-8 for safe validation")
    })
}

fn split_option(argument: &str) -> (&str, Option<&str>) {
    argument
        .split_once('=')
        .map_or((argument, None), |(name, value)| (name, Some(value)))
}

fn validate_value(
    provider: &ProviderKind,
    option: &str,
    value: &str,
    accepted: &[&str],
) -> Result<()> {
    if value.is_empty() || value.contains('\0') {
        bail!("native {provider} option `{option}` requires a non-empty value");
    }
    if value.starts_with('-') {
        bail!(
            "native {provider} option `{option}` value `{value}` looks like another flag; refusing an ambiguous invocation"
        );
    }
    if !accepted.is_empty() && !accepted.contains(&value) {
        bail!(
            "native {provider} option `{option}` has unsupported value `{value}` (allowed: {})",
            accepted.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn accepts_common_claude_options_in_separate_and_attached_forms() {
        validate(
            &ProviderKind::Claude,
            &strings(&[
                "--model",
                "sonnet",
                "--effort=high",
                "--permission-mode",
                "plan",
                "--verbose",
                "--no-chrome",
                "--debug=api,hooks",
                "-d=tools",
                "--prompt-suggestions=false",
            ]),
        )
        .expect("safe Claude options should pass");
    }

    #[test]
    fn accepts_common_codex_options_and_short_value_aliases() {
        validate(
            &ProviderKind::Codex,
            &strings(&[
                "--model=o3",
                "-s",
                "workspace-write",
                "-a",
                "on-request",
                "--search",
                "--no-alt-screen",
                "--strict-config",
            ]),
        )
        .expect("safe Codex options should pass");
    }

    #[test]
    fn empty_forwarding_is_valid() {
        validate(&ProviderKind::Claude, &[]).expect("empty Claude arguments");
        validate(&ProviderKind::Codex, &[]).expect("empty Codex arguments");
        preflight(None, &[]).expect("empty automatic arguments");
    }

    #[test]
    fn automatic_provider_preflight_accepts_only_the_allowlist_intersection() {
        preflight(None, &strings(&["--model", "sonnet"]))
            .expect("the shared model option is safe for either provider");

        let claude_only = preflight(None, &strings(&["--ax-screen-reader"]))
            .unwrap_err()
            .to_string();
        assert!(claude_only.contains("automatic provider selection"));
        assert!(claude_only.contains("select a provider explicitly"));
        assert!(claude_only.contains("Codex:"));

        let codex_only = preflight(None, &strings(&["--no-alt-screen"]))
            .unwrap_err()
            .to_string();
        assert!(codex_only.contains("automatic provider selection"));
        assert!(codex_only.contains("Claude:"));
    }

    #[test]
    fn explicit_provider_preflight_uses_only_that_provider_allowlist() {
        preflight(
            Some(&ProviderKind::Claude),
            &strings(&["--ax-screen-reader"]),
        )
        .expect("explicit Claude option");
        preflight(Some(&ProviderKind::Codex), &strings(&["--no-alt-screen"]))
            .expect("explicit Codex option");
    }

    #[test]
    fn positional_prompts_and_provider_subcommands_are_rejected() {
        for provider in [ProviderKind::Claude, ProviderKind::Codex] {
            assert!(validate(&provider, &strings(&["fix", "the", "tests"])).is_err());
            assert!(validate(&provider, &strings(&["--", "fix tests"])).is_err());
        }
        assert!(validate(&ProviderKind::Claude, &strings(&["--verbose", "fix tests"])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["--search", "fix tests"])).is_err());
        assert!(validate(&ProviderKind::Claude, &strings(&["doctor"])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["exec"])).is_err());
    }

    #[test]
    fn missing_or_flag_shaped_values_are_rejected() {
        assert!(validate(&ProviderKind::Claude, &strings(&["--model"])).is_err());
        assert!(validate(&ProviderKind::Claude, &strings(&["--model", "--verbose"])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["--model="])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["--sandbox", "--search"])).is_err());
    }

    #[test]
    fn optional_values_are_attached_only_so_prompts_cannot_be_consumed() {
        validate(&ProviderKind::Claude, &strings(&["--debug"]))
            .expect("bare optional debug filter is safe");
        assert!(validate(&ProviderKind::Claude, &strings(&["--debug", "fix this"])).is_err());
        assert!(
            validate(
                &ProviderKind::Claude,
                &strings(&["--prompt-suggestions", "false"])
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_and_compact_short_options_fail_closed() {
        assert!(validate(&ProviderKind::Claude, &strings(&["--future-flag"])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["--future-flag=x"])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["-mo3"])).is_err());
        assert!(validate(&ProviderKind::Claude, &strings(&["-dapi"])).is_err());
    }

    #[test]
    fn agentctl_owned_session_workspace_settings_and_lifecycle_flags_are_rejected() {
        let claude_cases: &[&[&str]] = &[
            &["--resume", "foreign"],
            &["--session-id=00000000-0000-0000-0000-000000000000"],
            &["--settings", "/tmp/settings.json"],
            &["--append-system-prompt=override"],
            &["--worktree", "other"],
            &["--print", "prompt"],
            &["--background"],
            &["--mcp-config", "servers.json"],
        ];
        for args in claude_cases {
            assert!(validate(&ProviderKind::Claude, &strings(args)).is_err());
        }

        let codex_cases: &[&[&str]] = &[
            &["--config", "model=o3"],
            &["-C", "/tmp/other"],
            &["--remote=ws://host"],
            &["--profile", "other"],
            &["--add-dir", "/tmp"],
            &["--dangerously-bypass-approvals-and-sandbox"],
        ];
        for args in codex_cases {
            assert!(validate(&ProviderKind::Codex, &strings(args)).is_err());
        }
    }

    #[test]
    fn variadic_or_initial_turn_options_are_rejected_conservatively() {
        assert!(
            validate(
                &ProviderKind::Claude,
                &strings(&["--allowed-tools", "Read", "Edit"])
            )
            .is_err()
        );
        assert!(
            validate(
                &ProviderKind::Claude,
                &strings(&["--file", "file_1:a", "file_2:b"])
            )
            .is_err()
        );
        assert!(
            validate(
                &ProviderKind::Codex,
                &strings(&["--image", "one.png", "two.png"])
            )
            .is_err()
        );
    }

    #[test]
    fn constrained_values_fail_closed() {
        assert!(
            validate(
                &ProviderKind::Claude,
                &strings(&["--permission-mode", "bypassPermissions"])
            )
            .is_err()
        );
        assert!(validate(&ProviderKind::Claude, &strings(&["--effort", "infinite"])).is_err());
        assert!(validate(&ProviderKind::Codex, &strings(&["--sandbox", "unknown"])).is_err());
    }

    #[test]
    fn plugins_have_no_native_argument_surface() {
        assert!(validate(&ProviderKind::Plugin("test".to_owned()), &[]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_arguments_fail_closed() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![b'-', b'-', b'm', b'o', b'd', b'e', b'l', 0xff]);
        assert!(validate(&ProviderKind::Codex, &[invalid]).is_err());
    }
}
