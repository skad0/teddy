# S9 Phase 2 benchmark results

## Methodology context
- Methodology: `s9-c1-post-readiness-observer-1`; C1 identity/PGID observation is deferred until after the named readiness sentinel.
- Historical context: The prior canonical Phase 2 C1 result was FAIL and used pre-drain identity/PGID observation; it remains immutable historical context and is not silently overwritten.
- C1 elapsed time starts at the post-fork harness clock; it is not complete process-launch latency.

## Executive summary
Run: `python3 bench/bench.py full --allow-large`
Source: `6da62c4ae917d541580a8ad856f83660a0cb5f9f` (dirty: `True`)
Cargo package: `0.2.0`
Runtime `teddy --version`: `teddy 0.2.0` (exit `0`)
Artifact bundle: `bench/artifacts/phase2-20260803T191923Z-0f1ca01656`

## Claim summary

| Claim | Status | Criterion / limitation | Evidence |
|---|---|---|---|
| C1 | **PASS** | p95 < 50 ms for both profiles; post-readiness observer boundary; validated teddy-only warm PTY repetitions; predeclared p95 threshold is 50ms | `C1 repetition artifacts` |
| C2 | **INCONCLUSIVE** | action-associated perf p95 < 1000 us; runtime perf PTY attempt; action association is required for PASS | `runtime attempt artifacts` |
| C3 | **INCONCLUSIVE** | observable literal search and cancellation; real literal-search/cancellation PTY attempts retained; UI action association is not established | `runtime artifacts` |
| C4 | **PASS** | source unchanged and X+original digest; isolated teddy integration and declared fixture digest verification | `fixture evidence` |
| C5 | **NOT_MEASURED** | report-only RSS; no budget; RSS samples are diagnostic only; no performance budget | `bench/artifacts/phase2-20260803T191923Z-0f1ca01656/c5-rss.json` |

## C1 repetitions

| Profile | Repetitions | p50 (ms) | p95 (ms) |
|---|---:|---:|---:|
| bare | 5 | 4.022 | 4.287 |
| shipped | 5 | 3.457 | 6.855 |

## Editor metric summary

| Editor / profile | Repetitions | p50 (ms) | p95 (ms) | Status | Comparability caveat |
|---|---:|---:|---:|---|---|
| Teddy / bare | 5 | 4.022 | 4.287 | PASS | Teddy C1 claim; comparator observations do not affect C1–C5. |
| Teddy / shipped | 5 | 3.457 | 6.855 | PASS | Teddy C1 claim; comparator observations do not affect C1–C5. |
| nvim | 2 | 1436.614 | 1555.598 | INCONCLUSIVE | Editor runtime/config differs from Teddy; non-comparable open-screen observation; does not affect C1–C5. |
| vim | 2 | 1962.795 | 1969.769 | INCONCLUSIVE | Editor runtime/config differs from Teddy; non-comparable open-screen observation; does not affect C1–C5. |
| hx | 2 | 2622.801 | 2630.612 | INCONCLUSIVE | Editor runtime/config differs from Teddy; non-comparable open-screen observation; does not affect C1–C5. |
| kak | 2 | 1259.932 | 1271.204 | INCONCLUSIVE | Kakoune two-process/server model; editor runtime/config differs; non-comparable open-screen observation. |
| less | 2 | 46.869 | 47.790 | INCONCLUSIVE | less is a demand-driven pager; editor runtime/config differs; non-comparable open-screen observation. |

## Runtime claim details
- **C2:** 2 runtime attempts; 2 parsed perf lines; p95 `2030.4` us; action association `False`.
- **C3:** 2 PTY attempts; named prompt/search/cancel actions retained; semantic association `False`.
- **C4:** 4/4 fixture/profile digest checks passed.
- **C5:** 10 workload sample records; RSS is diagnostic only and remains `NOT_MEASURED`.

## Comparators

| Tool | Status | Identity | Version probe |
|---|---|---|---|
| nvim | IDENTITY_RECORDED | `/opt/homebrew/Cellar/neovim/0.12.2/bin/nvim` `d3c3ac14d241` | exit `0` |
| vim | IDENTITY_RECORDED | `/usr/bin/vim` `0e7f7d3ff46a` | exit `0` |
| vi | ALIAS_OF | `/usr/bin/vim` `-` | exit `-` |
| hx | IDENTITY_RECORDED | `/opt/homebrew/Cellar/helix/25.07.1/bin/hx` `dc1e5d1c9d41` | exit `0` |
| kak | IDENTITY_RECORDED | `/opt/homebrew/Cellar/kakoune/2026.05.21/bin/kak` `94124dff5253` | exit `0` |
| less | IDENTITY_RECORDED | `/opt/homebrew/Cellar/less/704/bin/less` `8ad5a03aefdc` | exit `0` |
| vis | REJECTED_UNSUPPORTED | `/usr/bin/vis` `3249eebe64f7` | exit `1` |

## Comparator comparison

| Tool | Status | Exact identity | Invocation class | Repetitions / p50 / p95 (ms) | Caveat |
|---|---|---|---|---|---|
| nvim | INCONCLUSIVE | `/opt/homebrew/Cellar/neovim/0.12.2/bin/nvim` | `nvim` | 2 / 1436.614 / 1555.598 | editor runtime/config differs from teddy; open-screen observation only, not a teddy claim |
| vim | INCONCLUSIVE | `/usr/bin/vim` | `vim` | 2 / 1962.795 / 1969.769 | editor runtime/config differs from teddy; open-screen observation only, not a teddy claim |
| vi | ALIAS_OF | `/usr/bin/vim` | `vi` | unavailable | alias/rejected identity; not comparable |
| hx | INCONCLUSIVE | `/opt/homebrew/Cellar/helix/25.07.1/bin/hx` | `hx` | 2 / 2622.801 / 2630.612 | editor runtime/config differs from teddy; open-screen observation only, not a teddy claim |
| kak | INCONCLUSIVE | `/opt/homebrew/Cellar/kakoune/2026.05.21/bin/kak` | `kak` | 2 / 1259.932 / 1271.204 | Kakoune two-process/server model; editor runtime/config differs; open-screen only, not a teddy claim |
| less | INCONCLUSIVE | `/opt/homebrew/Cellar/less/704/bin/less` | `less` | 2 / 46.869 / 47.790 | less is a demand-driven pager; editor runtime/config differs; open-screen only, not a teddy claim |
| vis | REJECTED_UNSUPPORTED | `/usr/bin/vis` | `vis` | unavailable | unavailable, unsupported, or rejected; not comparable |

## Reproducibility

- Command: `python3 bench/bench.py full --allow-large`
- Bundle: `bench/artifacts/phase2-20260803T191923Z-0f1ca01656`
- Geometry: `200x50`; repetitions `5`; quiescence `5 ms`
- Environment: `3` sanitized variables; host `macOS-26.5.2-arm64-arm-64bit-Mach-O`, kernel `25.5.0`, arch `arm64`
- Evidence paths and SHA-256 values are recorded in the canonical JSON and remain immutable.

## Limitations

- PTY evidence measures application emission/transport and terminal-model screens, not physical rendering.
- C1 uses warm-cache runs without cache purge; identity/PGID probes are post-readiness and elapsed time is not complete process-launch latency.
- C2/C3 remain INCONCLUSIVE where action semantics cannot be established; C5 has no performance budget.
