# Documentation

The documentation is organized around the native CLI bridge boundary and the
operational guarantees that `agentctl` owns.

## Design and behavior

- [Architecture](architecture.md) — canonical sessions, native provider
  projections, workspace state, and failover.
- [Native provider boundaries](protocols.md) — Codex app-server and Claude Code
  stream-json contracts.
- [Compatibility policy](compatibility.md) — probes, capability degradation,
  fixtures, and version changes.
- [Protocol fixture corpus](../fixtures/README.md) — redacted, versioned native
  frames replayed by parser tests.
- [Security and privacy](security.md) — local data, credentials, redaction, and
  process isolation.

## Extending and shipping

- [Provider plugins](plugin-authoring.md) — process plugin protocol and manifest
  requirements.
- [Releasing](releasing.md) — release validation and artifact publication.
- [Commercial distribution gate](commercial-distribution.md) — authentication,
  provider approval, and branding constraints.

## Architecture decisions

- [ADR 0001: Canonical event log](adr/0001-canonical-event-log.md)
- [ADR 0002: One writer per worktree](adr/0002-single-writer.md)
- [ADR 0003: Process-isolated plugins](adr/0003-process-plugins.md)
