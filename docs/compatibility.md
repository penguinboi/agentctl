# Compatibility policy

Provider version strings are diagnostic input, not proof that native capture or
handoff is safe. Compatibility is established from the installed executable's
actual protocol surface and persisted capability evidence.

`agentctl doctor` performs non-model checks where possible:

- provider binaries are present and executable;
- Codex app-server initializes and can generate its installed JSON Schema;
- required native session and history methods are present;
- Claude Code supports the documented hook command form used by the bridge;
- state directories, SQLite migrations, and workspace locks are usable.

`agentctl doctor --live` may create disposable diagnostic sessions and execute
non-interactive protocol probes or real disposable turns, so it may consume
provider quota. It can behaviorally test create, resume, handoff, capture,
interruption, and persistence across restart. These probes are compatibility
tests, not a user conversation surface. Live evidence is bound to the exact
installed provider version and becomes stale after an upgrade.

A provider is launch-compatible only when `agentctl` can prove all capabilities
required for that direction of transfer. Optional telemetry may degrade to
unknown. Missing safe context delivery, native identity validation, or capture
must fail closed; it must never be replaced by an unrecorded prompt prefix or a
fabricated projection receipt.

Codex schemas are generated lazily and cached under the selected agentctl state
root at `protocols/codex/<normalized-installed-version>`. `--home` and
`AGENTCTL_HOME` therefore isolate both runtime and doctor protocol state; both
paths use the same version normalization, interprocess lock, validation, and
atomic publication. Running doctor first is not required. Claude hook and
resume behavior is capability-gated rather than inferred from help text alone.
Neither provider's private transcript files are a compatibility surface.

After upgrading `claude` or `codex`:

1. Exit every mapped native session.
2. Run `agentctl doctor`.
3. Run `agentctl doctor --live` only if the report requires behavioral proof.
4. Do not switch providers while a launch is marked uncertain; run
   `agentctl repair --abandon-native-launch LAUNCH_ID --rebuild-projections`
   only after confirming its provider process has exited, then inspect the
   canonical history and worktree before continuing. Any unresolved launch
   without a recorded PID remains fail-closed and cannot be reconciled or
   abandoned.

Redacted provider-version fixtures, transport fakes, and parser tests exercise
the compatibility boundary without quota use in ordinary CI. Together they
cover malformed JSON recovery, duplicate and out-of-order frames, unknown
items, abrupt exit, quota exhaustion, repeated capture, and versioned native
turn shapes. The checked-in corpus is documented in
[`../fixtures/README.md`](../fixtures/README.md).
