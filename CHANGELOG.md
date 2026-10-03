# Changelog

All notable changes to teddy are documented here. This file follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
Semantic Versioning.

## [Unreleased]

### Added

- Panics write a crash log with a backtrace to
  `$XDG_STATE_HOME/teddy/crash-<pid>.log` (default `~/.local/state`) and print
  its path after the terminal is restored (spec §20).

## [0.2.0]

### Added

- S0 core skeleton: terminal lifecycle, input parsing, rendering, modes, and
  basic file/quit handling.
- S1 byte storage: pread-backed chunk caching, piece-chain editing storage,
  byte-range transactions, and invalid-byte rendering.
- S2 editing: insert/delete, selections, undo/redo, atomic save, dirty-close
  prompting, and read-only mode.
- S3 huge-file behavior: large-file status flags, byte-window scrolling,
  streaming literal search, and progressive newline indexing.
- S4 renderer hardening: dirty regions, viewport caching, style runs, themes,
  and scroll-region optimization.
- S5 filesystem support: file watching, follow mode, reload notices, save
  guards, advisory locking, and file operations.
- S6 palette and picker: typed command arguments, lazy tree browsing, and
  ignore-file/global ignore support.
- S7 plugin host: framed stdio protocol, handshake, request IDs, revisions,
  stale-result handling, widgets, and restart support.
- S8 first-party plugin shells: Rust and Markdown highlighting, session and
  recovery support, an LSP shell, and an AI command/chat shell.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
