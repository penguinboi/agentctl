# Protocol fixtures

Only redacted captures may be committed. Provider-version-specific JSONL lives
under `codex/<version>/` and `claude/<version>/`; parser tests consume those
files directly as the single compatibility corpus. Ordinary CI replays these
fixtures and never invokes a model.
