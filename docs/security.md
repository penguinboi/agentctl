# Security and privacy

`agentctl` never reads provider tokens. Authentication remains entirely in the
installed Claude Code and Codex CLIs. The foreground provider also retains its
normal permission prompts, sandbox behavior, MCP configuration, tools, and
hooks. The wrapper does not enable a provider bypass mode.

## Wrapper and workspace boundary

A mapped native session must be opened through `agentctl`. The wrapper pins its
native session ID and worktree, records the child process, and holds one
exclusive writer lease throughout the foreground process and normal capture.
The child is attached to its process tree and its PID is committed before it
receives the foreground terminal.
If that lifecycle is interrupted, its unresolved outcome is journaled before
the lease is released; a later operation must reacquire the lease and reconcile
it. Running the same mapped session directly or concurrently is out of band and
can make safe migration impossible.

Forwarded arguments use an explicit provider allowlist. Arguments that replace
the mapped session, working directory, settings overlay, security boundary, or
foreground lifecycle are rejected. Positional arguments are rejected because
they are native initial prompts; unknown options remain disabled until their
arity and ownership are reviewed. Session-changing commands inside a provider
are also outside the supported contract: exit and use `agentctl` instead. A
detected native-session mismatch blocks the bridge and journals the launch as
failed or uncertain.

Claude hook invocations are untrusted process input. They are accepted only for
the exact open launch, canonical session, native session UUID, provider, and
stable worktree lease key. Input size and JSON depth are bounded, paths are
validated, ANSI control sequences are neutralized, and canonical payloads pass
through redaction before persistence. Oversized tool input and output are
reduced to bounded summaries, selected fields, byte counts, and content digests
rather than stored verbatim.

The Codex control connection is opened only around projection and capture. The
native thread is reconciled through official APIs after foreground exit; its
private rollout files are never edited. Unknown or incomplete history shapes do
not advance the canonical cursor.

## Side effects and interruption

Only one provider-owned native CLI may write a worktree at a time. A switch
begins only after the previous foreground process exits and its delta is
captured. `sync`, destructive maintenance, and projection rebuilds must not run
against a live mapped session.

`agentctl` never submits or automatically replays a user's conversational
request. Automatic routing may open the other native CLI only after the first
implicitly selected foreground process exits and its capture is confirmed; the
user decides what to send next. After a command, file mutation, background
process, migration, or external call, recovery preserves the side-effect state
so the next native turn can continue from the current workspace rather than
repeat work blindly.

Termination and capture are journaled separately. If the parent process, hook,
or machine fails in the delivery/capture window, the launch remains uncertain
until official provider state is reconciled. Uncertain handoffs block another
transfer instead of guessing whether context was delivered.

Pre/post native Git snapshots provide an additional conservative record of
observable repository changes. They do not prove the absence of ignored-file,
non-Git, process, or external effects. Explicitly abandoning an exited or
uncertain launch therefore requires a simultaneous projection rebuild and a
dead recorded process; it never converts uncertainty into a claim that no side
effect occurred. An unresolved launch without a recorded PID can be neither
reconciled nor abandoned, because that state is ambiguous across a spawn-time
wrapper crash.

## Sensitive local state

The canonical database can contain code-related text, prompts, assistant
responses, commands, file paths, provider frames, and usage data. On Unix,
state directories are mode 0700 and sensitive files are mode 0600. Treat the
entire state directory as source material.

Built-in and configurable secret patterns, payload size/depth limits, and ANSI
neutralization are applied before canonical persistence. Raw provider events
are retained separately for audit and parser diagnostics, but are not assumed
safe to share. No cookies, provider access tokens, or native credential files
are copied into the store.

Create a portable export with an additional best-effort redaction pass:

```bash
agentctl export SESSION_ID session-redacted.jsonl --include-blobs --redact
```

Inspect any export before sharing it. Debug logs are separate from the
canonical transcript and follow the configured retention policy.

## Configuration trust

The user-owned global `~/.agentctl/config.toml` may configure provider binary
paths, retention, and redaction. A repository-owned `.agentctl.toml` is limited
to allowlisted routing fields. Unknown or forbidden project fields fail closed,
so a checkout cannot replace provider executables or weaken local data policy.

Process plugins execute as the current OS user and are not a sandbox. Their
environment is rebuilt from a small portability allowlist plus explicit
manifest entries to reduce accidental token inheritance, but a trusted plugin
can still access the user's files, commands, and network. Plugins are not
eligible for the built-in native Claude/Codex launch workflow.
