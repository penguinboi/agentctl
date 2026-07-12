# ADR 0002: One writer per worktree

Status: accepted

Only one provider-owned native CLI may hold the exclusive writer lease for a
worktree. The lease begins before the foreground provider process starts and is
released only after the native lifecycle outcome is durably journaled. A later
launch must reconcile any non-captured outcome while holding a newly acquired
lease. No second provider may open concurrently in that worktree.

`agentctl` never submits or replays conversational prompts. After possible or
confirmed side effects, recovery must preserve uncertainty and continue from
the current worktree through a user-directed native turn.
