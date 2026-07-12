# Contributing

Use the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml) and
keep provider-specific wire formats outside `agentctl-core`. Before opening a
pull request, run:

```bash
bash -n scripts/native-e2e-smoke.sh
shellcheck scripts/native-e2e-smoke.sh
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo doc --locked --workspace --no-deps
cargo deny check
cargo audit --deny warnings
```

Never commit credentials or unredacted provider transcripts. New protocol
fixtures must document their provider version and pass the redaction checks.

## Native CLI bridge smoke test

The opt-in PTY smoke test exercises the user-facing bridge boundary with the
installed native CLIs:

```bash
scripts/native-e2e-smoke.sh
```

It creates an isolated Git worktree and `AGENTCTL_HOME`, opens Claude Code,
switches to Codex, returns to Claude Code, verifies the exclusive writer lock,
runs synchronization twice, and checks canonical history origins and provider
cursors. Because Claude receives a native handoff only when the next real
`UserPromptSubmit` fires, this no-model smoke expects Claude's final delta to
remain pending and verifies that a second sync does not duplicate or advance
it. It never submits a conversational prompt and therefore does not
intentionally start a model turn. Workspace snapshot events are produced by
the test process changing disposable files while each native UI is open.

The test requires configured and authenticated `claude` and `codex` binaries,
plus `expect`, `jq`, and `git`. Both native CLIs must already be through their
first-run onboarding. Set `AGENTCTL_NATIVE_E2E_KEEP=1` to retain the temporary
workspace, state, and PTY logs after a successful run, or `AGENTCTL_BIN` to test
a specific build.
