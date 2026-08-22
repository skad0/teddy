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

## Shared caveats

These apply to every measurement in this document and are not repeated below.

- All PTY numbers describe application emission, PTY transport, and
  terminal-model screen state. They are not physical rendering, and no claim is
  made about a display or a terminal emulator.
- Comparator and universal observations are never teddy claims: they cannot
  read, write, derive, or revise C1–C5 evidence, and no rankings are produced.
- Editor runtime and configuration differ between participants. Kakoune uses a
  server/UI process model and `less` is a demand-driven pager, so neither is
  equivalent to an editor open.

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
and environment metadata. Artifact paths use the repository-relative root
`bench/artifacts/`, resolvable from the repository root and independent of
result-output location. No generated result or canonical report belongs in the
repository during Phase 1.

Each smoke run uses a unique run directory below `bench/artifacts/`; result
evidence records that exact root, so later smoke/test runs cannot overwrite an
earlier result bundle. Tests may inject an isolated temporary artifact root.

Phase 2 is explicit and opt-in:

```sh
python3 bench/bench.py full --allow-large
python3 bench/run.sh full --allow-large
```

It generates the canonical result only after validating all claim outcomes and
immutable evidence. Phase 2 remains teddy-only for C1–C5. Identity-qualified
comparators now execute as separate bounded open-screen observations and never
affect teddy claims; unavailable, unsupported, aliased, or inconclusive tools
remain honestly non-comparable.

The current Phase 2 report is generated from one non-dry run. C2 requires a
runtime `TEDDY_PERF` log and structured PTY action evidence; C3 requires named
search-prompt and cancellation actions (otherwise it is INCONCLUSIVE); C5
records live root/PGID RSS samples but is always `NOT_MEASURED` because no RSS
budget is claimed. Comparator discovery records individual identities and
explicitly rejects `/usr/bin/vis`.

The historical pre-drain canonical run reported C1 `FAIL` because a valid
profile reached or exceeded the 50 ms p95 threshold; that failure remains
immutable historical context. The current post-readiness canonical run is C1
`PASS`. Under either version, invalid repetitions derive C1 `INCONCLUSIVE`,
regardless of stored claim status. C3 requires
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

Comparator attempt and aggregate statuses are likewise derived from their
materialized artifacts, never trusted from canonical JSON. A comparator PASS
requires exact argv/process-group identity and the `S9_C1_ROW_000000` corpus
sentinel at the named readiness endpoint (a filename alone is insufficient),
clean exit and reaping, both channel EOF/drain completion, no timeout, cap,
unsupported output, cleanup error, remaining PGID, or descendants, and valid
endpoint timing. Reports mark aliases and rejections rather than presenting
them as comparable teddy measurements.
Kakoune discovery probes `-version`; all seven names are retained exactly once,
and each record is cross-checked against its discovery path, alias, status,
invocation class, and argv.

The accepted C1 correction is versioned as `s9-c1-post-readiness-observer-1`.
Canonical Phase 2 C1 repetitions defer identity and PGID observation until
after the named `S9_C1_ROW_000000` readiness endpoint. Elapsed time begins at
the post-fork harness clock and is application-emission/PTY readiness time,
not complete process-launch latency. Strict staged identity, lifecycle, EOF,
drain, cleanup, PGID, corpus, and artifact checks remain claim inputs.

The historical canonical C1 `FAIL` result that used pre-drain identity/PGID
observation remains immutable historical context and is not silently
overwritten; the corrected canonical JSON uses a new result schema and records
that context explicitly. The separate ignored `c1-attribution --allow-large`
diagnostic remains methodology evidence only. It uses unchanged staged teddy
binaries and the deterministic 1 GiB corpus, with current pre-drain probes
compared against deferred post-readiness probes in five warmups and two
interleaved 31-repetition blocks per profile/mode. Observer contamination is
classified only when all paired samples are fully valid, deferred p95 improves
by at least 5 ms, and paired reduction agrees with removed probe time within
20%; missing, duplicate, or invalid reps yield an inconclusive methodology
result.

## Universal cross-editor comparison — Phase 1 contract

Each persisted phase artifact contains the phase, immutable rows, PTY-output
ordinal, complete trace-event index, causal output time, and associated input
trace-event index/write time (startup input fields are null). It is constructed
only from `until`/`write_endpoint` endpoint returns and hash-bound through the
phase map. Executor phase evidence adds exact replay and strict causal ordering;
it never bypasses raw projection, lifecycle, identity, helper, topology,
terminal, corpus, or timing validation.

The executor captures each action's pre-write screen only after quiescence and
immediately before input. Search `prompt_echo_ms` is the matched prompt-output
time, distinct from the prompt input-write time. Missing prompt, needle, or
submit endpoints are completed `INCONCLUSIVE` observations with explicit
unmatched phase records and hash-bound raw, trace, stderr, screen, topology,
and identity evidence; absent output ordinals are represented, never indexed.
Only exactly 32 valid PASS attempts derive a `MEASURED` metric.

The separate command is `python3 bench/bench.py compare --allow-large`. Its
versioned schema is `teddy-s9-universal-comparison-1`, its result is
`bench/results/comparison.json`, and its pure report is
`docs/bench_comparison.md`. It does not read, write, derive, or revise Teddy
C1–C5 canonical evidence.

The fixed participant order is Teddy shipped/default, nvim, vim, hx, kak,
less, vi as an exact vim alias, and `/usr/bin/vis` as
`REJECTED_UNSUPPORTED`; bare Teddy is omitted. Identity is absolute
path/hash/size/architecture/version, argv is direct and shell-free, and all
participants use isolated sanitized environments, a 200×50 PTY, deterministic
DSR/DA replies, and read-only corpus checks. Teddy's adapter is exactly
`[absolute-teddy, absolute-corpus]`; it does not use a nonexistent
`--read-only` option. The generated corpus is chmod'd read-only before launch;
manager/server setup is disabled.

The universal corpus is a deterministic UTF-8 1 GiB stream of 64-byte LF
records with unique head, 512 MiB needle/target, and tail records. Validation
streams the hash and marker offsets. Startup measures from `pre_fork_ms`, captured
before fork/exec, to the causal full head event; search measures submit-to-causal
full target event after prompt setup.
The target must be absent at startup and prompt echo cannot satisfy it.

Phase 1 records the contract and validator; the production executor is
available as `execute_universal_schedule()` at a full seven-adapter and
full smoke-matrix boundary; it derives the smoke-qualified subset internally. It launches one real `Session` per
coordinate, persists raw/filtered PTY trace, replay-bound phase and endpoint
screens, action writes, lifecycle, observed process-group rows, and before /
after executable identity. A completed result is rejected unless every
warmup and measured coordinate is present and causally bound; the second
measured block is reversed.

The checked-in result is now an executed one: `teddy-shipped` is `MEASURED`
for both operations on 32 valid attempts each, and every other participant is
an explicit `INCONCLUSIVE` row carrying its exclusion reason. Only
`teddy-shipped` passed the eligibility smoke. The other five drive terminal
features this screen model does not implement — mouse tracking, bracketed
paste, cursor shape, DECRQM — so their sessions do not end cleanly, and `less`
never matched the head marker. That is a limit of the harness, not a
measurement of those editors: they are ineligible, not slow. Making them
measurable means implementing those features in the screen model, not
loosening the smoke.

`compare --allow-large --execute` is gated by the Phase 1 Oracle, which is a
runnable adversarial check rather than a scheduled human review:
`test_oracle_mutation_probes_are_rejected` in `bench/tests/test_bench.py`.
The oracle mutates a valid materialized result and asserts each inconsistency
is rejected:

- a headline `elapsed_ms` detached from the validated timestamp chain;
- a chain shifted so it stays internally consistent, caught only by anchoring
  `endpoint_ms` and `submit_ms` to trace-bound event times and pinning
  `pre_fork_ms` to the trace clock origin — elapsed is a subtraction, so both
  of its operands need attesting, not just one;
- stripped process-lifecycle fields, or an absent `signal`, read as a clean
  exit — on both the attempt and the smoke path, since smoke `PASS` is what
  gates measurement eligibility;
- a self-attested executable digest never rehashed against disk, and its
  mirror, a binary substituted behind an honest digest;
- a result claiming `execution_state: not_started` while carrying metrics;
- a `contract_only` scaffold carrying attempts that reach a metric.

Each probe re-materializes its raw artifact so it is rejected by the check it
targets rather than by the raw-hash binding.

All of these were live holes found by adversarial review of this work; the
oracle is the regression suite for them. It is only meaningful while it stays
sensitive, so it is verified by reverting each fix in turn and confirming the
oracle fails — a probe that passes against weakened validation is worthless,
and two probes were rewritten after that sweep showed they were being caught
by unrelated checks. The gate is: the oracle passes, and every check it covers
still fails when removed. A newly found accepted-inconsistency class is a new
probe before execution, not after.
Every adapter declaration includes exact argv, search-prompt, submit, and quit
bytes plus expected topology/class. `vi` is accepted only as an exact vim
identity alias; `/usr/bin/vis` is always rejected.
Completed attempts require phase-map, participant-identity, and helper-identity
snapshots; the retired phase-less representation is rejected. Inconclusive
attempts retain explicit unattempted phase records, and their exact action and
write prefix is derived from the first missing phase and checked against the
filtered PTY input trace. Smoke evidence applies the same artifact-derived
rules; zero-attempt smoke rows remain status-only.

There are three retained, artifact-backed warmups per eligible adapter/operation
and two globally scheduled, rotated 16-repetition blocks per operation. The
schedule records contiguous indices, warmup/block/rep, and adapter/operation;
warmups never enter metrics. These are comparable only for the narrow shared
read-only startup and search operation. Only 32 valid attempts expose p50/p95
as `MEASURED`; otherwise the
operation is `INCONCLUSIVE`. Attempts retain raw JSON and hash/size descriptors,
trace/stderr/screen/endpoint artifacts, causal timestamps, separated prompt
echo, post-submit target evidence, deterministic terminal replies marked
harness traffic, unchanged corpus hashes, and complete EOF/drain/cap/process
cleanup evidence. Missing or duplicate entries, warmup contamination, forged
status/metrics, stale or pre-submit events, and topology mismatches are
rejected.
The single attempt contract is `teddy-s9-universal-attempt-1`; raw JSON,
full screen, endpoint screen, and retained process-topology artifacts are
resolved relative to the declared artifact root, rehashed, parsed, and checked
for exact canonical equality.

Before Phase 2 eligibility, `compare --allow-large` runs a bounded per-adapter
small-fixture PTY smoke using the declared prompt, literal needle, submit, and
quit bytes. A failed smoke records `INCONCLUSIVE` and cannot manufacture
participant metrics. The contract-only scaffold retains zero metric attempts;
the smoke is eligibility evidence only. Trace artifacts retain timestamped
PTY input/output events for parser replay. Smoke artifacts also retain
hash-bound startup/head, prompt, typed-needle, and post-submit target snapshots,
each tied to its causal output event; the final post-quit screen is validated
separately and need not contain prompt or needle text. Terminal DSR/DA traffic
is optional, but when present is validated separately from action writes.
