# ADR 0001: Canonical event log

Status: accepted

The agentctl append-only event log is the source of truth for events observed
while provider-owned interactive CLIs run through the wrapper. Provider
transcripts are projections because public provider contracts cannot represent
identical histories symmetrically. Projection receipts are unique by provider
session, canonical event, and projection version.

This decision does not make agentctl a conversation surface. Users submit every
request inside the native Claude Code or Codex process; agentctl only captures
and projects the resulting public delta.
