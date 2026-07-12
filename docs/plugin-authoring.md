# Provider plugins

> Process plugins are not launch targets for the built-in native conversation
> bridge. `agentctl open` and `agentctl switch` support only the installed
> Claude Code and Codex interactive CLIs. This document covers the lower-level
> management protocol retained for future extensions.

Provider plugins are executables speaking versioned JSON-RPC/JSONL over stdio.
They negotiate capabilities during `initialize`; Rust dynamic-library ABI is
not part of the public contract. Message definitions live in the
`agentctl-plugin-protocol` crate. A source consumer can call
`protocol_schema_bundle()` or `write_protocol_schema(path)` to generate the
exact JSON Schema bundle for that checkout. No standalone schema file is
currently shipped as a stable compatibility promise while this extension
surface remains experimental.

## Manifest

Install a plugin with `agentctl plugin install ./plugin.toml`. Relative
executable paths are resolved against the source manifest and canonicalized at
installation time.

```toml
manifest_version = 1
name = "example-agent"
version = "1.0.0"
protocol_versions = [1]
executable = "./bin/example-agent"
args = ["serve"]

[env]
PLUGIN_LOG = "warn"

[capabilities]
approvals = true
streaming = true
context_sync = true
interruption = true
```

Names must begin with a lowercase ASCII letter and contain only lowercase
letters, digits, `-`, or `_`. The CLI refuses non-executable files, manifest
symlinks, unsupported protocol versions, and manifest environment keys that
look like persisted credentials.

## Process environment

The host calls `env_clear` before spawning a plugin. It inherits only:

```text
COMSPEC HOME LANG LC_ALL LC_CTYPE LOGNAME PATH PATHEXT SHELL SYSTEMROOT
TEMP TMP TMPDIR TZ USER WINDIR
```

Manifest `[env]` values are applied after that allowlist, so an explicit value
can override an inherited one. Do not put credentials in the manifest: it is a
mode-0600 local file, not a secret store. Use an OS credential service or an
authentication flow owned by the plugin when a provider requires credentials.

Environment clearing prevents accidental leakage of parent-process secrets. It
does not isolate the filesystem, network, process namespace, or current user.
Plugins are trusted native executables; users must inspect them before install.

## Transport contract

The transport is one JSON-RPC 2.0 object per stdout line. Stdout is protocol
only; diagnostics belong on stderr. The host enforces line and request timeouts
and negotiates a mutually supported protocol version during `initialize`.
Version 1 covers probe, ensure/resume session, context synchronization, turn
start/event streaming, interruption, approval responses, and shutdown.

Run `agentctl plugin doctor NAME` after installation and after upgrading the
host or plugin. A plugin crash cannot corrupt the Rust host process, but any
workspace side effects already performed are treated with the same uncertainty
and current-worktree recovery policy as built-in providers.
