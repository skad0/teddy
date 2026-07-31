# Issue rules

Short taxonomy; apply labels on triage, one area max.

## Labels

| Label            | Use                                                        |
| ---------------- | ---------------------------------------------------------- |
| `bug`            | Behavior differs from spec/expectation. Needs repro.       |
| `enhancement`    | New behavior. Must pass the §21 spec check in the template.|
| `stage`          | Work item from `docs/plan.md` stages.                      |
| `area:core`      | Buffer, piece chain, transactions, input.                  |
| `area:render`    | Renderer, viewport cache, themes.                          |
| `area:plugin`    | Plugin host and first-party plugins.                       |
| `area:fs`        | Watchers, follow mode, atomic save.                        |
| `perf`           | Regression against a budget (open <50 ms, sub-ms frame…).  |
| `spec-violation` | Proposal/change conflicts with spec §21. Grounds to close. |
| `needs-repro`    | Missing steps, file, or version. Close if stale 14 days.   |

Plus GitHub defaults (`duplicate`, `wontfix`, `good first issue`, …).

## Workflow

1. File via template (`bug_report` / `feature_request`); blank issues off.
2. Triage: assign one `area:*`, add `stage` if it maps to the plan, drop
   `needs-repro` once repro is confirmed.
3. Anything hitting §21 gets `spec-violation` and a pointer to the spec.
4. PRs use the template checklist: `cargo test` green, exit check, Codex
   review, no §21 tech, `ponytail:` comments on shortcuts.
