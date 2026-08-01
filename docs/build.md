# Build instructions — teddy

Prerequisites
- Rust toolchain (rustc + cargo). Recommended stable toolchain (edition = 2021).
- Unix-like environment (teddy uses rustix and low-level terminal APIs).
- A C toolchain optionally for cross-building, and strip if you want small release artifacts.

Clone
- git clone git@github.com:skad0/teddy.git
- cd teddy

Regular development build
- cargo build

Release build
- cargo build --release

Produced binaries
- `teddy` — the editor
- `teddy-ai` — bundled sibling framed plugin shell
- `teddy-demo`, `teddy-highlight`, `teddy-lsp`, `teddy-manager`, and `teddy-session` — other bundled sibling executables

These binaries are targets of the single `teddytor` package; `teddy-ai` is not
a separate crate or catalog manager package. `cargo install teddytor` installs
the package's binaries as supported by Cargo.

Optimized small release (recommended)
- cargo build --release
Notes:
- Cargo.toml sets profile.release: opt-level = "z", lto = true, codegen-units = 1 and strip = true to produce compact, optimized binaries.

Perf build
- cargo build --release --features perf
- At runtime set TEDDY_PERF to a path to write perf frames:
  - TEDDY_PERF=/tmp/teddy-perf.log ./target/release/teddy

Packaging / reproducible tips
- The project minimizes external dependencies; static linking is not configured here by default.
- Check the `rustix` dependency features in Cargo.toml — only the required rustix subsets are enabled.

Running tests
- cargo test
