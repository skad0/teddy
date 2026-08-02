# Teddy S9 benchmark plan — Phase 1 and Phase 2

Phase 1 is a real, non-dry, bounded harness-health smoke. It does not publish
benchmark claims or canonical results. The public commands are:

```sh
python3 bench/bench.py test
python3 bench/bench.py smoke -o /tmp/teddy-s9-phase1.json
python3 bench/bench.py report /tmp/teddy-s9-phase1.json -o /tmp/teddy-s9-phase1.md
git diff --check
cargo test --all-targets
```

`bench/run.sh` is only a wrapper for these commands. There is no `run`,
`full`, dry-run mode, comparator, arbitrary-executable runner, 1 GiB workload,
secondary geometry, or generic terminal-emulation promise in Phase 1.

Every result has `phase: "phase1"` and schema `teddy-s9-phase1-result-5`.
C1–C5 are always `NOT_MEASURED`; Phase 1 cannot turn a claim into PASS. C1
1 GiB/p95, C2 perf logs, C3 search/cancel, C5 process-tree interpretation,
comparators, and large fixtures are Phase 2 work. The binary renderer
reproducer is an observed diagnostic (including its exit or signal), never a
predeclared `FAIL`, and is not a claim.

## Fixtures

`bench/corpora.json` is authoritative. `smoke` generates five fixed small
fixtures and one aggregate `manifest.json`, recording exact size, SHA-256,
properties, screen markers, and post-edit digest. `code-1m.rs` is exactly
16,384 64-byte source-like lines and contains head/page/tail sentinels.

The edit fixtures include exact CRLF/no-final-newline and invalid-byte/no-NUL
cases. `binary-safe.bin` contains a NUL within its first 8KiB, uses a
content-only sentinel, and has escaped-byte evidence; `RO` and `BIN` are
independently required on status row 49;
`binary-render-repro.bin` ends with a truncated UTF-8 lead byte. The 1 GiB
log is reserved in the manifest and is never generated or run in Phase 1.

## Smoke and evidence

Release `target/release/teddy` and `target/release/teddy-highlight` are
staged into isolated profiles. Bare contains exactly `teddy`; shipped contains
exactly `teddy` and `teddy-highlight`, with hashes and sizes recorded. Both
profiles run fixed open, PageDown, edit/save-copy, and binary-read required
scenarios. The truncated-UTF-8 renderer reproducer runs separately as an
observed diagnostic. HOME, every XDG root, TMPDIR, CWD, TERM, and LANG are isolated.
Phase 1 uses a controlling 200×50 PTY; secondary geometry is deferred.

The persistent session waits for alternate-screen, filename/status, and head
sentinel readiness before actions. Its incremental screen model supports
CR/LF/BS/tab, printable output, CSI movement/erase/SGR, save/restore, and
private cursor/alternate-buffer modes (`?25`, `?47`, `?1047`, `?1049`).
Unsupported sequences are `INCONCLUSIVE`.

PageDown sends `CSI 6~` and requires an action endpoint containing the stable
source-fixture rows `S9_ROW_00001` through `S9_ROW_00048`; the initial head is
never accepted as the post-action endpoint.

Records include named startup/readiness and action write, the output-event
sequence at write, first-output strictly after write, endpoint-match at its
causal PTY output event, last-output, and quiet-complete timestamps; quiet
completes only after a full 5ms with no PTY byte and is excluded from latency.
PTY and stderr are independently capped and continuously drained. The retained
trace is capped, while its SHA-256 and full/retained/discarded byte counts cover
the complete drained PTY stream. Raw traces and screen records are ignored
generated artifacts. Timeout, cap, unsupported sequence, missing endpoint,
cleanup error, abnormal exit, nonzero exit, or a drain deadline before both
channel EOF/EIO events cannot be PASS.
Process groups are killed and reaped on all paths; Darwin PTY EIO is handled.
Main RSS is diagnostic only.

Required health excludes the renderer reproducer; it is an observed diagnostic
with exit/signal/stderr/source location. Reports separate `harness_status` from
`application_status` and diagnostic-record-derived `application_diagnostic_status`, retain every
non-PASS reason, and include process, terminal, action, artifact, corpus,
profile, hash/size, version, commit/dirty/UTC, host, locale, geometry, limits,
and environment metadata. PTY evidence describes application emission and PTY
transport, not a physical display or emulator. Artifact paths use the
repository-relative root `bench/artifacts/`, explicitly resolvable from the repository root and independent of result-output
location. No generated result or
canonical report belongs in the repository during Phase 1.

Each smoke run uses a unique run directory below `bench/artifacts/`; result
evidence records that exact root, so later smoke/test runs cannot overwrite an
earlier result bundle. Tests may inject an isolated temporary artifact root.

Phase 2 is explicit and opt-in:

```sh
python3 bench/bench.py full --allow-large
python3 bench/run.sh full --allow-large
```

It generates the canonical result only after validating all claim outcomes and
immutable evidence. Phase 2 remains teddy-only; optional comparators are
discovery-only and never affect teddy claims.

The current Phase 2 report is generated from one non-dry run. C2 requires a
runtime `TEDDY_PERF` log and structured PTY action evidence; C3 requires named
search-prompt and cancellation actions (otherwise it is INCONCLUSIVE); C5
records live root/PGID RSS samples but is always `NOT_MEASURED` because no RSS
budget is claimed. Comparator discovery records individual identities and
explicitly rejects `/usr/bin/vis`. PTY timestamps describe application
emission/transport and terminal-model screens, not physical rendering.

The current canonical run reports C1 `FAIL` when all repetitions are valid but
either profile reaches or exceeds the 50 ms p95 threshold; this is a measured
failure, not an inconclusive harness result. Any invalid repetition instead
derives C1 `INCONCLUSIVE`, regardless of the stored claim status. C3 requires
Enter-submitted search evidence distinct from the prompt before association can
be claimed. C4 keeps the actual saved-output bytes and digest for every
fixture/profile and rehashes each saved artifact, requiring the computed hash,
retained hash, actual hash, and expected hash to be identical. Canonical-copy
mutation tests cover C1/C3/C4 evidence detachment and stale report numbers.
Phase 2 environment capture is an explicit nonsecret allowlist only. C2 is
derived from parsed, rehashed log artifacts: incomplete or unassociated
attempts are `INCONCLUSIVE`; complete association derives `PASS` below 1000 us
p95 and `FAIL` otherwise. C3 is `PASS` only when both complete attempts have
artifact-backed semantic association, and otherwise `INCONCLUSIVE`. C4 derives
`PASS` only from four validated PASS fixtures and `FAIL` from any validated
fixture failure; malformed matrices are rejected. Each C2 attempt also retains
a `c2-<profile>-attempt.json` artifact, which is hash-checked and compared
exactly, including `association`, before derivation. C4 fixture statuses are
derived from source-unchanged and computed/recorded/actual/expected saved
digest equality; stored fixture and aggregate statuses cannot override them.
