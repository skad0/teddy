# S9 universal cross-editor comparison

Separate from Teddy C1–C5: universal observations never modify or contribute to those claims.

**Completed execution.** Results below are evidence-bound observations only; no rankings or C1–C5 claims are produced. Phase 2 eligibility requires both `IDENTITY_QUALIFIED` identity and `PASS` adapter smoke; smoke-ineligible adapters remain explicit `INCONCLUSIVE` rows.

## Metrics

| Adapter | Operation | Repetitions | p50 (ms) | p95 (ms) | Status | Caveat |
|---|---|---:|---:|---:|---|---|
| teddy-shipped | search | 32 | 304.71516647958197 | 315.60420015011914 | MEASURED | Narrow shared read-only startup/search operation; separate from C1–C5. |
| teddy-shipped | startup | 32 | 5.031916516600177 | 6.615925030200742 | MEASURED | Narrow shared read-only startup/search operation; separate from C1–C5. |
| nvim | search | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| nvim | startup | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vim | search | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vim | startup | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| hx | search | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| hx | startup | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| kak | search | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Kakoune uses a server/UI process model; narrow operation only, separate from C1–C5. |
| kak | startup | 0 | — | — | INCONCLUSIVE | Smoke ineligible (quit/cleanup lifecycle failed); Kakoune uses a server/UI process model; narrow operation only, separate from C1–C5. |
| less | search | 0 | — | — | INCONCLUSIVE | Smoke ineligible (startup/readiness or topology identity failed); less is a demand-driven pager, not an editor; narrow operation only, separate from C1–C5. |
| less | startup | 0 | — | — | INCONCLUSIVE | Smoke ineligible (startup/readiness or topology identity failed); less is a demand-driven pager, not an editor; narrow operation only, separate from C1–C5. |
| vi | search | 0 | — | — | ALIAS_OF | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vi | startup | 0 | — | — | ALIAS_OF | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vis | search | 0 | — | — | REJECTED_UNSUPPORTED | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |
| vis | startup | 0 | — | — | REJECTED_UNSUPPORTED | Smoke ineligible (not an identity-qualified participant); Narrow shared read-only startup/search operation; runtime/config differences remain, separate from C1–C5. |

## Contract
- Schema: `teddy-s9-universal-comparison-1`; geometry `200x50`; warmups `3`; two rotated blocks of 16 measured repetitions (warmups excluded).
- Each eligible adapter/operation retains three warmups plus a contiguous global schedule for the two rotated 16-repetition blocks; warmups are excluded from metrics. Startup uses pre-fork-to-head timing; search uses submit-to-target timing.
- Only 32 valid causal attempts expose headline p50/p95; no rankings are produced.
- PTY metrics describe application emission and terminal-model events, not physical rendering.
- Phase 1 runs a bounded per-adapter small-fixture smoke for eligibility; smoke statuses are diagnostic and do not create participant metrics.
- Teddy uses the documented positional invocation only: `[teddy, corpus]`; the corpus is read-only on disk and manager mode is disabled. Its shipped topology is the staged Teddy root plus the exact sibling `teddy-highlight` helper. `vi` must be an exact vim alias and `/usr/bin/vis` is rejected.
- Phase 2 must validate raw-attempt, timestamped trace replay, artifact, causal timing, terminal-traffic, corpus-integrity, topology, and cleanup evidence before an operation becomes `MEASURED`.
