# Releasing agentctl

GitHub binary releases are the only supported package publication channel.
Workspace packages use `publish = false`, so crates.io publication is
deliberately disabled while the internal crate boundaries and plugin protocol
remain pre-1.0.

## Release checklist

1. Update `workspace.package.version` in `Cargo.toml` and `Cargo.lock`.
2. Update `CHANGELOG.md` and compatibility notes.
3. Run the complete CI suite, including an explicit Rust 1.88.0 MSRV check,
   workspace tests on Linux, macOS, and Windows, `cargo-deny`, and
   `cargo-audit`.
4. Create and push the exact tag `v<workspace-version>`.
5. Verify the GitHub Release contains four archives (Linux x86_64 musl, macOS
   x86_64/arm64, and Windows x86_64) and their SHA-256 files.
6. Verify each archive contains `agentctl`/`agentctl.exe`, `LICENSE`, and
   `README.md`.

The release workflow calls the same reusable CI workflow used by pushes and
pull requests. Platform builds depend on that workflow and an independent
tag/version check, then upload immutable workflow artifacts. One final release
job downloads all four archives, verifies all four SHA-256 files, checks the
expected eight-file bundle, and publishes a single GitHub Release with generated
notes. GitHub Actions are pinned to full commit SHAs; updates must be resolved
from the official action repository and reviewed before changing a pin.
`cargo audit --deny warnings` makes vulnerability, unsoundness, and
unmaintained-dependency notices release-blocking rather than informational.
