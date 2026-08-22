# S9 universal cross-editor comparison

Separate from Teddy C1–C5: universal observations never modify or contribute to those claims.

**Completed execution.** Results below are evidence-bound observations only; no rankings or C1–C5 claims are produced. Phase 2 eligibility requires both `IDENTITY_QUALIFIED` identity and `PASS` adapter smoke; smoke-ineligible adapters remain explicit `INCONCLUSIVE` rows.

## Metrics

| Adapter | Operation | Repetitions | p50 (ms) | p95 (ms) | Status | Caveat |
|---|---|---:|---:|---:|---|---|
| teddy-shipped | search | 32 | 308.26350051211193 | 346.3497123448178 | MEASURED | Narrow shared read-only startup/search operation; separate from C1–C5. |
| teddy-shipped | startup | 32 | 5.749145522713661 | 7.3251418536528945 | MEASURED | Narrow shared read-only startup/search operation; separate from C1–C5. |
| nvim | search | 32 | 1132.844000007026 | 1192.7046937169507 | MEASURED | Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| nvim | startup | 32 | 1167.1523544937372 | 1329.9626045220066 | MEASURED | Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vim | search | 32 | 1325.3888124600053 | 1444.407404368394 | MEASURED | Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vim | startup | 32 | 1859.748250019038 | 1984.1635124728782 | MEASURED | Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| hx | search | 32 | — | — | INCONCLUSIVE | 32/32 attempts exceeded the harness timeout; Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| hx | startup | 32 | 591.9323540001642 | 670.9726414701436 | MEASURED | Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| kak | search | 32 | 1076.7540834494866 | 1195.953877735883 | MEASURED | Kakoune uses a server/UI process model; narrow operation only, separate from C1–C5. |
| kak | startup | 32 | 1272.9285409732256 | 1347.3204651236301 | MEASURED | Kakoune uses a server/UI process model; narrow operation only, separate from C1–C5. |
| less | search | 32 | 7376.3635414943565 | 7597.403610593756 | MEASURED | less is a demand-driven pager, not an editor; narrow operation only, separate from C1–C5. |
| less | startup | 32 | 6.932749995030463 | 9.229216995299794 | MEASURED | less is a demand-driven pager, not an editor; narrow operation only, separate from C1–C5. |
| vi | search | 0 | — | — | ALIAS_OF | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vi | startup | 0 | — | — | ALIAS_OF | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vis | search | 0 | — | — | REJECTED_UNSUPPORTED | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vis | startup | 0 | — | — | REJECTED_UNSUPPORTED | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |

## Contract
- **This published result is a summary.** It carries derived metrics only. The per-attempt evidence (420 attempts, 59.7 MB) is too large for the repository and is retained in the run bundle at `/Users/kriant/w/teddy/bench/artifacts/comparison-20260822T120305Z-28094a8ce1/result-full.json`, bound to this summary by SHA-256 `5093113d3501c010…`. Re-run `compare --allow-large --execute` to regenerate it.
- Schema: `teddy-s9-universal-comparison-summary-1`; geometry `200x50`; warmups `3`; two rotated blocks of 16 measured repetitions (warmups excluded).
- Each eligible adapter/operation retains three warmups plus a contiguous global schedule for the two rotated 16-repetition blocks; warmups are excluded from metrics. Startup uses pre-fork-to-head timing; search uses submit-to-target timing.
- Only 32 valid causal attempts expose headline p50/p95; no rankings are produced.
- PTY metrics describe application emission and terminal-model events, not physical rendering.
- Phase 1 runs a bounded per-adapter small-fixture smoke for eligibility; smoke statuses are diagnostic and do not create participant metrics.
- Teddy uses the documented positional invocation only: `[teddy, corpus]`; the corpus is read-only on disk and manager mode is disabled. Its shipped topology is the staged Teddy root plus the exact sibling `teddy-highlight` helper. `vi` must be an exact vim alias and `/usr/bin/vis` is rejected.
- Phase 2 must validate raw-attempt, timestamped trace replay, artifact, causal timing, terminal-traffic, corpus-integrity, topology, and cleanup evidence before an operation becomes `MEASURED`.
