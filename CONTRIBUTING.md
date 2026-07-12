# Contributing

Use stable Rust and keep provider-specific wire formats outside `agentctl-core`.
Before opening a pull request, run:

```bash
cargo fmt --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo doc --locked --workspace --no-deps
cargo deny check
cargo audit --deny warnings
```

Never commit credentials or unredacted provider transcripts. New protocol
fixtures must document their provider version and pass the redaction checks.
