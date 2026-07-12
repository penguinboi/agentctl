# Changelog

All notable changes are documented here. This project follows Semantic
Versioning after 1.0.

## [Unreleased]

## [0.1.0] - 2026-07-12

- Initial implementation.
- Native-only workflow: agentctl prepares and captures sessions while the real
  Claude Code or Codex CLI owns the foreground terminal.
- Explicit `open`, `switch`, and `resume` launch boundaries with one worktree
  writer and canonical delta synchronization.
- GitHub binary-only release pipeline with reusable quality and supply-chain gates.
- Plugin subprocess environments are cleared and rebuilt from an explicit allowlist.
- Local metrics for canonical state, provider health, usage, and synchronization.
- Official-interface native-session attachment with explicit transcript limitations.
- Official Codex `thread/read` native-history import with sanitized raw separation,
  crash-safe item idempotency, and a fail-closed Claude limitation.
- Native Claude lifecycle capture through launch-bound hooks and structured
  cross-provider handoff context.
- Claude native-session materialization distinguishes an interrupted empty
  launch from a non-empty official transcript. Codex-to-Claude handoffs are
  delivered on the first real `UserPromptSubmit`; exiting without a prompt
  leaves the delta staged and its projection cursor unchanged.
- Sync-only Claude projection no longer treats a `shouldQuery:false` queue send
  as a durable provider receipt. Repeated sync and immediate teardown leave the
  cursor pending for the launch-bound `UserPromptSubmit` handoff.
- Claude `PreToolUse`/completion capture with normalized tool, command, and
  changed-file events; oversized hook output is bounded and content-digested.
- Pre/post native Git snapshots provide a conservative workspace-effect
  fallback when provider events are incomplete.
- Guarded recovery can explicitly abandon an exited or uncertain native launch
  while rebuilding projection state under the same workspace guard.
- Indexed native-hook identity and active-turn lookups avoid full event-log
  scans during normal Claude hook handling.
- Health-based automatic fallback after a captured native-process boundary,
  plus fail-closed mid-turn crash continuation after the journaled process is
  proven dead and provider/workspace evidence is durable. The opposite native
  CLI receives a deterministic non-replay capsule; agentctl never submits or
  replays conversational prompts.
- Fail-closed native option allowlists reject positional initial prompts and
  provider flags that can escape session, worktree, settings-overlay, capture,
  or foreground-lifecycle ownership. Explicit allowlisted native security
  options remain user-controlled and are never added automatically. Validation
  runs immediately after provider selection, before projection, provider
  binding, routing, launch journals, settings, or workspace snapshots mutate.
- Native PIDs are journaled before terminal handoff; every post-spawn wrapper
  failure remains `Uncertain`, and unresolved launches without a PID cannot be
  reconciled or abandoned.
- Removed the disconnected headless routing/approval engine, legacy replay
  state, unused public APIs, duplicate fixtures, and unnecessary dependency
  features before the first public release.
