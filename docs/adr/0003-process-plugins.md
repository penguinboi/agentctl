# ADR 0003: Process-isolated plugins

Status: accepted

Third-party providers use versioned JSON-RPC over stdio. Rust dynamic library
ABI is intentionally excluded so plugins can use any language and crash without
corrupting the host process.

Plugin subprocesses start from an empty environment and receive only a minimal
portability allowlist plus explicit manifest values. This reduces accidental
credential inheritance but is not an OS sandbox: plugins remain trusted code
with the invoking user's filesystem, process, and network authority.
