## What changed

Describe the user-visible behavior and the native provider boundary affected.

## Validation

- [ ] `bash -n scripts/native-e2e-smoke.sh`
- [ ] `shellcheck scripts/native-e2e-smoke.sh`
- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --locked --workspace --all-features`
- [ ] `cargo doc --locked --workspace --no-deps`
- [ ] Relevant provider fixture, protocol probe, or native PTY smoke completed

## Safety

- [ ] No credentials, private transcripts, or proprietary source were committed.
- [ ] The change preserves native prompt ownership and does not replay side effects blindly.
- [ ] Protocol-specific formats remain behind their provider adapter.
