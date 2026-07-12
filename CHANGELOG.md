# Changelog

All notable changes are documented here. This project follows Semantic
Versioning after 1.0.

## [Unreleased]

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
- Claude `PreToolUse`/completion capture with normalized tool, command, and
  changed-file events; oversized hook output is bounded and content-digested.
- Pre/post native Git snapshots provide a conservative workspace-effect
  fallback when provider events are incomplete.
- Guarded recovery can explicitly abandon an exited or uncertain native launch
  while rebuilding projection state under the same workspace guard.
- Indexed native-hook identity and active-turn lookups avoid full event-log
  scans during normal Claude hook handling.
- Health-based automatic fallback only after a captured native-process boundary;
  agentctl never submits or replays conversational prompts.
- Fail-closed native option allowlists reject positional initial prompts and
  provider flags that can escape session, worktree, capture, or security ownership.
- Native PIDs are journaled before terminal handoff; every post-spawn wrapper
  failure remains `Uncertain`, and unresolved launches without a PID cannot be
  reconciled or abandoned.
