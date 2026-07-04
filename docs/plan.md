# teddy — execution plan

Spec: `docs/raw_spec.md` (locked). This file is the operational map: stages,
exit checks, delegation, budget. Update checkboxes as stages land.

## Ground rules

- Ponytail full: shortest working diff, stdlib/rustix before crates, no
  speculative abstraction. Deliberate shortcuts get `// ponytail:` comments
  naming the ceiling and upgrade path.
- Byte-addressed everywhere. No mmap, no rope, no regex, no async, no TUI
  framework, no in-process plugins (spec §21 rejection list is law).
- Every stage ends green: `cargo test` passes + the stage's exit check runs.
- Every stage ends with a Codex review of the stage diff before commit.

## Stages

- [x] **S0 — core skeleton** (`main`, `term`, `input`, `render`)
  CLI (files / `-w`), raw mode + alt screen with panic-safe restore,
  escape-sequence input parser, single reusable frame buffer,
  tabline/statusline, empty buffer, cursor movement, Ctrl+Q.
  *Exit:* opens/quits cleanly in a pty, terminal restored on panic,
  parser unit tests pass, non-tty exits with an error.
- [x] **S1 — byte storage**
  pread chunk cache, chunked piece chain, add store, byte-range
  transactions, render real file rows, `\xNN` invalid-UTF-8 escapes.
  *Exit:* opens a multi-GB file instantly with correct first viewport;
  piece-chain + transaction unit tests; invalid bytes render escaped.
- [x] **S2 — editing**
  Insert/delete, selections, capped undo/redo, atomic save, dirty-close
  prompt, read-only mode.
  *Exit:* edit→save round-trip is byte-identical outside edits;
  undo/redo invertibility test.
- [x] **S3 — huge-file behavior**
  256 MiB threshold, status flags (HUGE/RO/NOIDX/BIN), byte-window
  scrolling, streaming literal search, replace-all disabled, progressive
  newline index.
  *Exit:* 1 GiB file opens <50 ms; search streams and cancels on input.
- [x] **S4 — renderer hardening**
  Dirty rows + region flags, viewport render cache, style runs, theme
  table, scroll-region optimization.
  *Exit:* dev-build instrumentation shows zero per-keypress allocation
  and sub-ms input→frame time on a large file.
- [x] **S5 — filesystem**
  kqueue/inotify watchers, follow mode, clean auto-reload, dirty-change
  notice, mtime/size save guard, advisory lock, file ops.
  *Exit:* external edit auto-reloads a clean buffer; follow tails a log.
- [x] **S6 — palette / picker**
  Command palette with typed args, lazy tree picker, expand-on-search,
  root `.gitignore` subset, global ignore globs.
  *Exit:* open a nested file via picker in a repo; ignores respected.
- [x] **S7 — plugin host**
  Binary framed stdio protocol, handshake, request IDs, resource
  revisions, stale-drop, structured widgets, restart API.
  *Exit:* demo plugin renders a list widget and submits a validated
  byte-range edit transaction.
- [ ] **S8 — first-party plugins**
  Rust + Markdown lexical highlighters, session/recovery, LSP shell,
  AI command/chat shell.
  *Exit:* highlighter plugin colors the visible viewport out-of-process.

## Delegation map (efficient-fable)

**Fable only** (architecture, coupled core, judgment): piece chain +
transaction engine, event loop, dirty/render model, save strategy, plugin
protocol framing design, all final integration and review triage.

**Cheap subagents** (bounded, independent, evidence-back): key-sequence
test corpora, `.gitignore` subset parser, streaming search, per-OS watcher
backends (one agent each), demo/highlighter plugin executables, benchmark
scripts, log/test-output reduction. Handoff packets carry: objective, files
in scope, evidence format, verification commands, stop conditions. Max 3
parallel. Reports are leads — Fable re-verifies cited lines before acting.

**Codex** (pair + review): `/codex:review` on every stage diff;
`/codex:adversarial-review` on the correctness-critical stages (S1 storage,
S2 undo/save, S7 protocol); `/codex:rescue` when a diagnosis stalls.

## Budget protocol (stay-within-limits)

Between stages (and between subagent waves):
`rtk proxy npx -y ccusage@latest blocks --active --json`.
At ≥95% of the 5-hour or weekly window: stop launching, schedule a wakeup
for `min(3600, until-clear)`, re-check on wake (compare block start IDs,
not wall clock). Wake prompts carry: remaining stages, this rule, and the
next stage's exit check.

## Cadence

One stage ≈ one session. Never start a stage that can't reach its exit
check inside the remaining window — a half-landed storage engine is worse
than an unstarted one.
