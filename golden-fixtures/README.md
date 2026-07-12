# Golden protocol fixtures

The versioned, redacted JSONL golden fixtures are maintained under
[`../fixtures`](../fixtures) so parser replay and chaos tests share one source
of truth. `fixtures/codex/<version>/`, `fixtures/claude/<version>/`, and
`fixtures/chaos/` are the committed Phase 0 compatibility corpus; no model call
is made when replaying them.
