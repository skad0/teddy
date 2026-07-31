# teddy

teddy is a single-threaded, byte-addressed, pread-backed terminal text editor implemented in Rust. It's designed for small, auditable code, predictable behavior, and a small plugin protocol for out-of-process helpers.

This repository contains the editor core. Key source files:
- src/main.rs — editor runtime, UI, modes and command palette.
- src/plugin.rs — plugin protocol and plugin process management.
- Cargo.toml / Cargo.lock — Rust build configuration.

This README covers:
- How to build
- Runtime options / env vars
- Plugin contract (summary); full protocol in docs/plugin-contract.md
- User-facing interface & commands in docs/interface.md
- Configuration spec in docs/config-spec.md
- Theme spec in docs/theme-spec.md

If you'd like these docs added to the repo, I can commit them to the docs branch (docs/) — tell me to commit or request edits first.
