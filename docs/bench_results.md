# S9 Phase 2 benchmark results

## Executive summary
Run: `python3 bench/bench.py full --allow-large`
Source: `4b7c96bef0e0522a076e800f1776aebf7d5cb347` (dirty: `True`)
Cargo package: `0.2.0`
Runtime `teddy --version`: `teddy 0.2.0` (exit `0`)
Artifact bundle: `bench/artifacts/phase2-20260802T195611Z-68200ea341`

## Claim summary

| Claim | Status | Criterion / limitation | Evidence |
|---|---|---|---|
| C1 | **FAIL** | p95 < 50 ms for both profiles; validated teddy-only warm PTY repetitions; predeclared p95 threshold is 50ms | `C1 repetition artifacts` |
| C2 | **INCONCLUSIVE** | action-associated perf p95 < 1000 us; runtime perf PTY attempt; action association is required for PASS | `runtime attempt artifacts` |
| C3 | **INCONCLUSIVE** | observable literal search and cancellation; real literal-search/cancellation PTY attempts retained; UI action association is not established | `runtime artifacts` |
| C4 | **PASS** | source unchanged and X+original digest; isolated teddy integration and declared fixture digest verification | `fixture evidence` |
| C5 | **NOT_MEASURED** | report-only RSS; no budget; RSS samples are diagnostic only; no performance budget | `bench/artifacts/phase2-20260802T195611Z-68200ea341/c5-rss.json` |

## C1 repetitions

| Profile | Repetitions | p50 (ms) | p95 (ms) |
|---|---:|---:|---:|
| bare | 5 | 46.243 | 46.943 |
| shipped | 5 | 48.454 | 77.301 |

## Runtime claim details
- **C2:** 2 runtime attempts; 2 parsed perf lines; p95 `2145.7` us; action association `False`.
- **C3:** 2 PTY attempts; named prompt/search/cancel actions retained; semantic association `False`.
- **C4:** 4/4 fixture/profile digest checks passed.
- **C5:** 10 workload sample records; RSS is diagnostic only and remains `NOT_MEASURED`.

## Comparators

| Tool | Status | Identity | Version probe |
|---|---|---|---|
| nvim | IDENTITY_RECORDED | `/opt/homebrew/Cellar/neovim/0.12.2/bin/nvim` `d3c3ac14d241` | exit `0` |
| vim | IDENTITY_RECORDED | `/usr/bin/vim` `0e7f7d3ff46a` | exit `0` |
| vi | IDENTITY_RECORDED | `/usr/bin/vim` `0e7f7d3ff46a` | exit `0` |
| less | IDENTITY_RECORDED | `/opt/homebrew/Cellar/less/704/bin/less` `8ad5a03aefdc` | exit `0` |
| hx | IDENTITY_RECORDED | `/opt/homebrew/Cellar/helix/25.07.1/bin/hx` `dc1e5d1c9d41` | exit `0` |
| kak | IDENTITY_RECORDED | `/opt/homebrew/Cellar/kakoune/2026.05.21/bin/kak` `94124dff5253` | exit `255` |
| vis | REJECTED_UNSUPPORTED | `/usr/bin/vis` `3249eebe64f7` | exit `1` |

## Reproducibility

- Command: `python3 bench/bench.py full --allow-large`
- Bundle: `bench/artifacts/phase2-20260802T195611Z-68200ea341`
- Geometry: `200x50`; repetitions `5`; quiescence `5 ms`
- Environment: `3` sanitized variables; host `macOS-26.5.2-arm64-arm-64bit-Mach-O`, kernel `25.5.0`, arch `arm64`
- Evidence paths and SHA-256 values are recorded in the canonical JSON and remain immutable.

## Limitations

- PTY evidence measures application emission/transport and terminal-model screens, not physical rendering.
- C1 uses warm-cache runs without cache purge.
- C2/C3 remain INCONCLUSIVE where action semantics cannot be established; C5 has no performance budget.
