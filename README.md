# teddy

teddy is a small, single-threaded, byte-addressed terminal text editor written in
Rust. It uses pread-backed storage and an out-of-process plugin protocol.

## Build and install

Build from a checkout with a stable Rust toolchain on a Unix-like system:

```sh
cargo build --release
```

Run the resulting `target/release/teddy` binary. To install the published
package, use `cargo install teddytor`; from a checkout, use
`cargo install --path .`. The project keeps its runtime dependency set
deliberately small.

The development build, release profile, and test commands are also documented
in [`docs/build.md`](docs/build.md).

## Documentation

- [User interface and commands](docs/interface.md)
- [Cheatsheet](docs/cheatsheet.md)
- [Plugin contract](docs/plugin-contract.md)
- [Configuration](docs/config-spec.md)
- [Themes](docs/theme-spec.md)
- [Benchmark methodology](docs/bench_plan.md) and [results](docs/bench_results.md)
- [Crate documentation](https://docs.rs/teddytor)

Plugins are independent executables communicating over teddy's framed stdin/
stdout protocol. The [plugin contract](docs/plugin-contract.md) is the
canonical protocol reference; it covers the handshake, messages, revisions,
widgets, and edit transactions.

The core also provides a bounded out-of-process launcher and registry. The
bundled `teddy-manager` is a separately launched manager: it runs only when
`TEDDY_PLUGIN_MANAGER` names an existing absolute executable. The core remains
the launcher and registry; it does no Git or package work and never accepts
shell, argv, or package authority.

The manager reads a local catalog and installed inventory, and offers explicit
cancel-first confirmations for install (which leaves the plugin disabled),
install-and-enable, enable, disable, and safe ordered remove. Accepted install
inputs are only a canonical public GitHub HTTPS repository URL, an exact
40-hex commit, a strict `teddy-plugin.v1` manifest, and a prebuilt executable.
Git commands and output are bounded; installations are immutable and receipt/
SHA-256 verified.
There are no builds, hooks, dependency or submodule handling, LFS, SSH or local
URLs, remote catalogs, automatic updates, or shell access. Updates are explicit
and confirmation-gated: the manager can switch an eligible enabled payload to a
different exact catalog pin and safely roll back if the candidate fails. It does
not delete the old payload. The manager cannot self-manage and has no sandbox,
signature, or self-update behavior.

See [configuration](docs/config-spec.md), the [manager guide](docs/plugin-manager.md),
[plugin recipes](docs/plugin-recipes.md), and the
[launcher contract](docs/plugin-contract.md) for setup and operation. Repository
maintainers should also read the [plugin repository contract](docs/plugin-repository-contract.md);
the [manifest reference](docs/plugin-manifest.md) remains normative.

The `teddytor` package also produces bundled sibling executables, including
`teddy-ai`. `teddy-ai` is an out-of-process framed plugin shell that registers
`ai` and currently replies `ai shell: no provider configured`; it has no AI
provider or functional AI configuration. See the [cheatsheet](docs/cheatsheet.md)
for direct use.

## Benchmarks

The harness lives in `bench/` and is plain Python 3 with no dependencies. It
drives real `teddy` processes over a controlling 200×50 PTY and derives every
number from hash-bound artifacts rather than from timing code's own reports.

```sh
python3 bench/bench.py test                       # harness self-tests
python3 bench/bench.py smoke -o /tmp/s9.json      # Phase 1: bounded health smoke
python3 bench/bench.py report /tmp/s9.json -o /tmp/s9.md
python3 -m unittest discover -s bench/tests -p 'test_*.py'
```

Phase 1 publishes no claims: every result carries `phase: "phase1"` and all
claims stay `NOT_MEASURED`. Phase 2 is explicit and opt-in
(`python3 bench/bench.py full --allow-large`); it generates the 1 GiB corpus
and is the only mode that can turn a claim into `PASS`.

Current Phase 2 status, from [`docs/bench_results.md`](docs/bench_results.md):

| Claim | Status | What it measures |
|---|---|---|
| C1 open latency | **PASS** | p95 < 50 ms on a 1 GiB file; bare 4.29 ms, shipped 6.86 ms |
| C2 frame latency | INCONCLUSIVE | perf p95 `2030.4` us parsed, but action association not established |
| C3 search/cancel | INCONCLUSIVE | real search and cancel attempts retained; semantics not associated |
| C4 save integrity | **PASS** | 4/4 fixture/profile digest checks; saved bytes byte-identical |
| C5 memory | NOT_MEASURED | RSS sampled for diagnosis only; no budget is claimed |

C1 elapsed time starts at the post-fork harness clock, so it is PTY-readiness
time rather than complete process-launch latency. See
[`docs/bench_plan.md`](docs/bench_plan.md) for the full methodology and its
shared caveats.

### Cross-editor comparison

`python3 bench/bench.py compare --allow-large --execute` runs a separate
universal comparison against nvim, vim, hx, kak, and less on a deterministic
1 GiB corpus: 32 measured repetitions per participant per operation, in two
rotated blocks, after three discarded warmups. Full results and caveats are in
[`docs/bench_comparison.md`](docs/bench_comparison.md).

| Participant | startup p50 | startup p95 | search p50 | search p95 |
|---|---:|---:|---:|---:|
| **teddy** | **5.75** | **7.33** | **308.26** | **346.35** |
| less | 6.93 | 9.23 | 7376.36 | 7597.40 |
| hx | 591.93 | 670.97 | — | — |
| nvim | 1167.15 | 1329.96 | 1132.84 | 1192.70 |
| kak | 1272.93 | 1347.32 | 1076.75 | 1195.95 |
| vim | 1859.75 | 1984.16 | 1325.39 | 1444.41 |

All values are milliseconds, all `MEASURED` on 32 valid attempts, except
`hx search`, which is `INCONCLUSIVE`: 32/32 attempts exceeded the harness
timeout, so Helix does not complete a 1 GiB search inside it.

Startup is pre-fork to the causal head event; search is submit to the causal
target event 512 MiB into the file. Every participant runs from an isolated
`HOME` and XDG roots, so no user configuration is loaded.

**Read the `less` row before drawing conclusions.** It is the control, not a
competitor: a demand-driven pager is the only other participant here that does
not ingest the file, and it starts in 6.93 ms against teddy's 5.75 ms. The
600–1900 ms startups belong to tools that read and index 1 GiB. So the startup
column mostly measures *whether a tool loads the file*, not how fast its code
is. Teddy's search result is the less derivative claim: 308 ms is 3.5× the
nearest editor and 24× `less`, on the same bytes.

**No rankings are produced**, and this is not a general editor benchmark — it
is two narrow read-only operations, on one corpus shape that suits teddy
(64-byte records), on one machine, with a warm page cache.

Execution is gated on an adversarial oracle,
`test_oracle_mutation_probes_are_rejected`, which mutates a valid result and
asserts each inconsistency is rejected — timings detached from the trace,
stripped lifecycle evidence, self-attested binary digests, and scaffolds
carrying metrics. The gate is that the oracle passes *and* every check it
covers still fails when removed, so the probes cannot quietly go blind.

## Releases

Releases are performed only by GitHub Actions after the change has been merged
to `main`; a local `cargo publish` is not the release procedure. Repository
administrators must configure these settings manually:

1. Create a protected `crates-io` environment containing the
   `CRATES_IO_TOKEN` secret and requiring an approval before deployment.
2. Create a tag ruleset that restricts creation and deletion of `v*` tags to
   the release maintainers or release automation.

These are manual repository settings; the workflow does not create or change
them. During a release, the workflow peels the tag and validates that its
commit is contained in `main` before publishing. A release tag should still be
pushed only after its commit has been merged into `main`.

To request a release after merging, create and push an exact version tag:

```sh
git tag -a v0.2.0 -m "teddy v0.2.0"
git push origin v0.2.0
```

The workflow validates the tag, package version, tests, package contents, and
changelog before publishing to crates.io and creating the GitHub release.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option.
