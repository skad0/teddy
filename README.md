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
