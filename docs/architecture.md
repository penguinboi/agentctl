# Architecture

`agentctl` is a control plane around provider-owned interactive CLIs. It does
not execute a user's conversational turn through an agentctl prompt surface.
For every working session, the user remains inside either Claude Code or Codex.

## Three kinds of state

The canonical append-only event store is the source of truth for information
observed by `agentctl`. It contains public prompts and results, effect summaries,
provider health, projection receipts, and native launch/capture journals.

Each provider also owns a native session: a Claude Code session UUID or Codex
thread ID. These sessions are projections of the canonical conversation, not
identical copies of it. The mapping is stable for the lifetime of a canonical
session.

Finally, the Git worktree is shared physical state. Both native providers see
the same files, diff, commands, and test artifacts. One exclusive lease covers
the entire foreground native-process lifetime so two writers cannot operate on
the same worktree concurrently.

## Native launch boundary

A launch follows this sequence:

1. Resolve the canonical session and workspace identity.
2. Acquire the worktree writer lease.
3. Reconcile any previously interrupted native launch while holding the lease.
4. Select the provider and project only its missing canonical delta.
5. Record a pre-launch Git workspace snapshot.
6. Spawn the real provider inside an owned process tree, durably journal its
   PID, then give it the foreground terminal with inherited I/O.
7. Let the provider own interaction, approvals, tools, and its agent loop.
8. After the provider exits, capture the native delta and a post-launch Git
   snapshot, then append confirmed observable changes canonically.
9. Mark the launch captured, then release the worktree lease.

`agentctl switch` performs the same sequence for another provider. The user
must first leave the currently running native CLI; switching is never a second
concurrent execution of the same request.

An automatic fallback is also a launch-boundary operation. After a cleanly
captured native exit, an implicitly selected launch may open the other
provider's native CLI while retaining the same worktree lease. A mid-turn crash
can cross providers only after the journaled process group is proven dead, the
provider delta and a post-crash workspace snapshot are durable, and any staged
handoff is known not to be in an ambiguous delivery state. The coordinator
then commits a deterministic `native_context_marker` with `Possible` or
`Confirmed` side effects and `replay_allowed=false` before closing the failed
launch. The abandoned execution turn becomes terminal `Failed` so it cannot be
mistaken for a live Claude turn, while its monotonic side-effect state and a
separate audit event preserve the uncertainty. It opens the other native CLI,
but never submits the previous prompt; the user decides what continuation to
send there. Explicit provider selections and launches with forwarded native
arguments do not auto-fallback.

## Capture and projection

Claude Code emits documented lifecycle hooks while its native interactive CLI
is running. `PreToolUse` records tool intent and marks possible side effects
before execution. Completion hooks record normalized tool outcomes; recognized
shell and file tools additionally produce command lifecycle and changed-file
events. The bridge also records public prompts, final assistant output,
failures, and session lifecycle events. A launch-specific identifier, native
session UUID, and stable worktree lease key bind every hook to the foreground
process that authorized it.

Hook idempotency and active-turn recovery use indexed native-hook identities
and turn status, avoiding a full canonical-event scan for each normal hook
invocation. Oversized hook inputs and results are reduced to bounded summaries,
selected operational fields, original byte counts, and content digests before
persistence.

Codex history is reconciled after native exit with the official app-server
`thread/read` surface. Stable native item identities make repeated capture
idempotent. Unsupported or incomplete item shapes fail closed rather than
silently advancing the projection cursor.

Before the other provider starts, the transcript layer builds a bounded
handoff capsule from the missing canonical events and current workspace
effects. It includes original requests, public results, decisions, changed
paths, command/test outcomes, and open work. It excludes private reasoning,
bulky duplicated output, and file contents already available on disk.

Git snapshots captured before and after every native provider launch record
HEAD, branch, changed paths, coverage, and a diff digest. When that observable
state changes, a bounded diff summary becomes a canonical projection event.
This is a fallback for effects missed by provider events, not proof that an
ignored path, non-Git file, background process, or external system was
unchanged.

## Crash and side-effect model

Native launches are journaled as `Started`, `CaptureReady`, `Exited`,
`Captured`, `Failed`, or `Uncertain`. `CaptureReady` is Claude-specific: its
`SessionEnd` hook has durably committed the captured delta, but the foreground
process has not yet been confirmed dead. Only the child-process exit promotes
that launch to `Captured`; `Exited` means the child ended without equivalent
final capture evidence. Neither intermediate state permits a concurrent
provider launch.

Projection receipts are unique by provider session, canonical event, and
projection version. Repeating a proven projection is therefore a no-op.
`agentctl` never retries or replays the user's conversational prompt. After a
command, file mutation, background process, or other possible effect, recovery
must preserve uncertainty and the next user-directed native turn must continue
from the observed workspace instead of repeating work blindly.

Crash reconciliation is idempotent. The continuation event ID is derived from
the canonical session and failed launch ID, and it is appended before the
launch becomes terminal. A second boot therefore either observes the same
durable marker or still sees an open launch and fails closed. A live process
group, missing PID receipt, missing workspace snapshot, changed Codex thread
identity, or Claude handoff in `Delivering`/`Uncertain` state prevents the
transition.

An unrecoverable `Exited` or `Uncertain` launch can be explicitly abandoned
only by combining `--abandon-native-launch LAUNCH_ID` with
`--rebuild-projections`. The repair operation first verifies that the recorded
process is no longer alive, reacquires the workspace mutation lease, marks the
launch failed, and rebuilds projection state under that same guard. Any open
launch without a child PID remains blocked from both reconciliation and
abandonment because the wrapper may have died after spawn but before journaling
the PID. Neither recovery path erases or disproves possible workspace side
effects.

Large retained outputs and diffs live in a SHA-256-addressed zstd blob store.
Normalized events drive the product while bounded, redacted raw frames support
protocol diagnostics and parser replay.
