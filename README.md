# agentctl

`agentctl` is a local launcher and conversation bridge for the installed
Claude Code and Codex CLIs.

You still work inside the real `claude` or `codex` interactive terminal. The
wrapper only prepares the provider session, opens that native CLI in the
foreground, records the public conversation and relevant workspace effects,
and projects the delta when you move to the other provider. It never accepts
chat prompts itself and does not replace either provider's interface, login,
tools, MCP servers, hooks, permissions, or agent loop.

> **Status:** pre-1.0. Native provider protocols change frequently. Run
> `agentctl doctor` after upgrading Claude Code or Codex.

## The workflow

Create a canonical session and open Claude Code:

```bash
cd your-project
agentctl doctor
agentctl new --name auth-race --provider claude
```

`agentctl` now gives the terminal to the installed `claude` binary. Use Claude
Code normally. When you want Codex to take over:

1. Finish the current native turn.
2. Exit Claude Code.
3. Run:

```bash
agentctl switch codex --session auth-race
```

The command projects the missing canonical context, then opens the installed
Codex CLI in the same worktree. To return to Claude Code, exit Codex and run:

```bash
agentctl switch claude --session auth-race
```

There is no live provider swap inside an active native process. An ordinary
switch occurs at a clean process boundary so `agentctl` can capture the
completed native delta before the other provider starts. If a native process
crashes mid-turn, a switch is allowed only after its journaled process group is
proven dead, the provider delta and post-crash workspace snapshot are
preserved, and a deterministic continuation capsule is committed.

Automatic failover obeys the same boundary. When the launch provider was
chosen implicitly, and only when no native arguments were forwarded,
`agentctl` may open the other native CLI after a clean captured exit
whose recorded health requires fallback. After a recoverable crash it may also
open the other native CLI with the durable continuation capsule, marked with
`Possible` or `Confirmed` side effects. It never moves provider execution,
types or replays the user's prompt, or follows an explicit provider selection
with another provider. The user submits the continuation inside the newly
opened native CLI.

Useful launch forms:

```bash
agentctl                         # open the current session's active provider
agentctl open claude             # open Claude for the current workspace session
agentctl open codex --session auth-race
agentctl resume auth-race        # reopen the session's active native provider
agentctl new --name prepared --no-launch
```

Arguments after `--` are forwarded only when they are recognized, bounded
native options that do not replace the wrapper-owned session, workspace,
settings overlay, capture hooks, or foreground lifecycle:

```bash
agentctl new --name auth-race --provider claude -- --model sonnet
agentctl open claude --session auth-race -- --model sonnet
agentctl open codex --session auth-race -- --model gpt-5
```

When the provider is implicit or `auto`, forwarded options must belong to the
intersection of the Claude and Codex allowlists. Select `--provider` (or use an
explicit provider argument on `open`/`switch`) for provider-specific flags such
as Claude `--ax-screen-reader` or Codex `--no-alt-screen`.

Explicitly forwarded native security options are still the user's decision.
For example, Codex `--sandbox` / `--ask-for-approval` and Claude
`--permission-mode` can make the native process less restrictive than its
defaults. `agentctl` never adds those options itself and continues to reject
the providers' all-in-one bypass flags.

`new` forwards those options to its initial native launch too. Native options
cannot be combined with `new --no-launch`, because no provider process exists
to receive them.

Positional arguments are always rejected because both provider CLIs interpret
one as an initial conversational prompt. Enter prompts only after the native
interface opens. Unknown options fail closed until their ownership and arity
are reviewed for the installed provider version.

Provider commands that create, fork, clear, or switch to another native session
must not be used while a mapped session is open. Exit the provider and use
`agentctl new`, `agentctl fork`, `agentctl open`, or `agentctl switch` so the
native identity and canonical history cannot diverge.

## Why the wrapper is required

Starting `claude` or `codex` directly is still possible, but those out-of-band
turns are not automatically part of an `agentctl` session. The reliable path is
always:

```text
agentctl prepares projection
  -> native provider owns the terminal
  -> user exits native provider
  -> agentctl captures and commits the delta
  -> next provider receives the missing context
```

Do not run the same mapped native session directly or from two terminals at
once. `agentctl` holds one exclusive writer lease for the worktree for the full
native process lifetime.

## What is transferred

The canonical event log records the public information needed for semantic
continuity:

- user requests and final assistant responses;
- public plans and decisions;
- tool and command outcomes that affect the task;
- changed-file paths and summarized workspace effects;
- errors, usage and rate-limit signals when the provider exposes them;
- projection receipts and native launch/capture state.

For Claude, launch-bound hooks observe tool intent and completion. Recognized
shell and file operations become normalized command and changed-file events.
`agentctl` also records Git snapshots immediately before and after every native
launch; an observable Git-state change is retained as a conservative fallback
when a provider hook does not describe the whole workspace effect.

It deliberately excludes private reasoning, giant logs, complete source files
that remain readable in the worktree, credentials, and duplicate raw output.
Oversized Claude hook fields are reduced to bounded summaries, selected
operational fields, byte counts, and SHA-256 digests instead of being persisted
verbatim. Large retained diagnostics are stored as content-addressed blobs.

The worktree is shared memory too: the next provider sees the current files,
Git diff, and test state. Context transfer therefore uses a bounded handoff
capsule instead of copying an ever-growing transcript into every launch.

### The histories are intentionally asymmetric

Codex exposes official APIs for reading a thread and injecting external user or
assistant messages. A Claude-to-Codex handoff can therefore be projected into
the Codex thread as real transcript messages without starting an extra model
turn.

Claude Code does not expose a public equivalent for inserting an arbitrary
external assistant message into an existing transcript. A Codex-to-Claude
handoff is supplied as structured historical context by the first real
`UserPromptSubmit` hook. Merely opening and exiting Claude does not consume the
handoff or advance its projection cursor; the delta remains pending for the
next prompt. Claude receives the meaning of the prior work, but its native
transcript is not a byte-for-byte copy of the Codex transcript.

`agentctl` promises one auditable canonical conversation and semantic
continuity, not two identical provider-owned histories. It never edits either
provider's private transcript files.

## Bringing an existing native chat under agentctl

First create an empty canonical session without launching a provider:

```bash
agentctl new --name imported-work --no-launch
```

### Existing Codex thread

Codex has an official history-read API, so `agentctl` can import the supported
public transcript into an empty canonical session:

```bash
agentctl import-native codex THREAD_ID \
  --session imported-work \
  --activate
agentctl resume imported-work
```

The import uses `thread/read(includeTurns=true)`, binds the thread to the same
worktree, excludes private reasoning and bulky outputs, and is idempotent by
stable native turn/item identity. It fails closed if the installed Codex
version returns a partial or unsupported history shape.

Use `attach` instead when you only want to continue the thread without copying
its older public history into the canonical log:

```bash
agentctl attach codex THREAD_ID --session imported-work --activate
```

### Existing Claude Code session

Claude Code does not provide a supported full transcript-read API. `agentctl`
can validate and resume an existing session UUID, but cannot retroactively
import its older transcript:

```bash
agentctl attach claude SESSION_UUID \
  --session imported-work \
  --activate
agentctl resume imported-work
```

The older history remains available inside Claude Code. New turns made through
the wrapper are captured canonically and can be transferred from that point
forward. If the older conversation is needed by Codex, open the attached Claude
session through `agentctl` and produce an explicit handoff summary as a new
turn; that new public response can then be projected. There is no lossless,
automatic import of the pre-attachment Claude transcript.

## Sessions and inspection

These commands manage state; none of them accepts a conversational prompt:

```bash
agentctl list
agentctl status auth-race
agentctl metrics auth-race
agentctl history auth-race
agentctl sync auth-race
agentctl compact auth-race
agentctl fork auth-race --name auth-race-alt
agentctl export auth-race session.jsonl --include-blobs --redact
agentctl repair
agentctl delete auth-race
```

`status` shows the active provider, provider health, native session IDs, sync
lag, and workspace state. `metrics` reads only local canonical data; provider
usage can be absent or incomplete when the native protocol does not report it.

`sync` projects pending Codex context without opening a native chat. Claude's
`shouldQuery:false` stream has no durable delivery receipt, so a sync-only
process never advances Claude's cursor merely because it enqueued that frame.
Its delta remains canonical and is journaled for delivery by the first real
`UserPromptSubmit` of the next native Claude launch; the sync report marks that
cursor as `next_native_user_prompt`. Normal switches already synchronize
lazily, so explicit sync is mostly useful before going offline or for
diagnostics. Never run state-changing maintenance while a mapped native CLI is
still open.

An `Exited` or `Uncertain` native launch is not silently discarded. After
confirming that its recorded provider process is no longer alive, explicitly
abandon it and rebuild projection state in the same guarded operation:

```bash
agentctl repair \
  --abandon-native-launch LAUNCH_ID \
  --rebuild-projections
```

The command reacquires the workspace mutation lease and fails closed if the
process is still running, the launch/workspace identity does not match, or
another unresolved launch blocks the rebuild. Any unresolved launch without a
journaled child PID is deliberately neither reconciled nor abandonable: a
wrapper crash could have happened in the small spawn-to-journal window, so the
absence of a PID is not proof that no provider survived. Recovery never asserts
that uncertain side effects did not happen; inspect canonical history and the
current worktree before the next native turn.

## Installation

Install and authenticate Claude Code and/or Codex separately first. `agentctl`
does not acquire, copy, refresh, or store provider credentials.

GitHub Release archives are the supported distribution channel. Download the
archive for your platform, verify its adjacent checksum, and place `agentctl`
on `PATH`:

```bash
shasum -a 256 -c agentctl-aarch64-apple-darwin.tar.gz.sha256
tar -xzf agentctl-aarch64-apple-darwin.tar.gz
install -m 0755 agentctl-aarch64-apple-darwin/agentctl "$HOME/.local/bin/agentctl"
```

The release matrix currently builds:

- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

To build from a checkout, install Rust 1.88 or newer and run:

```bash
cargo install --locked --path crates/cli
```

All workspace packages set `publish = false`; installation from crates.io is
not supported.

## Configuration and local state

Global configuration is read from `~/.agentctl/config.toml`. A project-local
`.agentctl.toml` may override only allowlisted routing fields. A repository
cannot replace native provider binaries or weaken retention and redaction.
Use `--home PATH` or `AGENTCTL_HOME` to replace the state directory.

```toml
retention_days = 30
redaction_patterns = ["ACME-[0-9]{4}"]

[routing]
policy = "sticky-balanced"
switch_threshold = 0.25
failure_window_seconds = 300
max_recent_failures = 2

[providers]
codex_binary = "codex"
claude_binary = "claude"
```

Routing policies are `manual`, `claude-first`, `codex-first`, `balanced`, and
`sticky-balanced`. Routing only selects which native CLI to open at a launch
boundary; it does not send a prompt to either provider. A health-based fallback
from an implicitly selected provider can open the other native CLI only after
the first foreground process exits and capture completes. A mid-turn crash
additionally requires a dead journaled PID, a durable post-crash snapshot,
unambiguous handoff state, and a canonical non-replay continuation marker. The
user still writes the next request inside that native CLI.

The state directory contains the SQLite canonical event store, blobs, protocol
capability evidence, workspace locks, and diagnostic logs. On Unix,
directories are mode 0700 and sensitive files are mode 0600. Treat the whole
directory as sensitive source material, not disposable cache.

The checked-in schemas describe the
[`global configuration`](schemas/config/agentctl.schema.json) and restricted
[`project configuration`](schemas/config/agentctl-project.schema.json).

## Compatibility and safety

`agentctl doctor` checks installed binaries, protocol surfaces, and local state
without intentionally starting a model turn. `agentctl doctor --live` uses
disposable sessions to test behavior and may consume provider quota.

Provider upgrades can invalidate capture or handoff capabilities. If safe
synchronization cannot be proved, the provider is marked incompatible or the
operation fails closed; canonical history is never fabricated. Run
`agentctl repair` after an interrupted launch when requested. For an
`Exited`/`Uncertain` launch, use the explicit abandonment and projection rebuild
form above, then inspect the session before continuing uncertain side effects.

The local database can contain prompts, source-related output, commands, and
responses. See [`docs/architecture.md`](docs/architecture.md),
[`docs/protocols.md`](docs/protocols.md),
[`docs/security.md`](docs/security.md),
[`docs/compatibility.md`](docs/compatibility.md), and
[`SECURITY.md`](SECURITY.md).

## Release policy

Tags must exactly match the workspace version (`v0.1.0` for version `0.1.0`).
A release is built only after the reusable CI workflow passes MSRV, formatting,
Clippy, tests, documentation, license/advisory policy, and vulnerability audit
checks. See [`docs/releasing.md`](docs/releasing.md).

## License

MIT
