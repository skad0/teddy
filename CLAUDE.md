# teddy

Byte-addressed, pread-backed terminal editor. Spec: `docs/raw_spec.md`
(locked — the §21 rejection list is law). Execution map: `docs/plan.md`
(stages, exit checks, delegation, budget protocol). Work stage by stage;
every stage ends with `cargo test` green, its exit check run, and a Codex
review of the stage diff.
