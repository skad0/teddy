# Plugin manager

`teddy-manager` is an optional, separately launched bundled binary. To use it,
set `TEDDY_PLUGIN_MANAGER` to its existing absolute executable path before
starting `teddy`:

```sh
export TEDDY_PLUGIN_MANAGER="$PWD/target/release/teddy-manager"
teddy file.txt
```

The path must be absolute, a regular file, and executable. The core does not
download or bootstrap the manager. Without this setting there is no manager
slot. The core remains only the launcher and registry; it does no Git, package,
build, hook, dependency, or shell work.

## Inspect and operate

Use Ctrl-P and choose:

* `plugins-catalog [query]` — local pinned catalog;
* `plugins-installed [query]` — local receipt-backed installations;
* `plugins-core [query]` — launcher desired and observed state;
* `plugins-refresh` — reload all three views.

Queries are case-insensitive substring filters. Select a catalog or installed
record to see its actions. Each install, install-and-enable, enable, disable,
and remove flow presents `Cancel` before its confirmation. Cancel, an expired
confirmation, a stale selection, or a changed source makes no change.

Install fetches the pinned public GitHub repository at its exact commit,
validates the strict manifest and prebuilt executable, and installs it disabled.
Install-and-enable performs the same install and then sends launcher `Enable`.
Enable and disable change desired registry state through the launcher. Remove
is deliberately ordered: `Disable`, bounded `List` polling until the record is
absent or disabled in `Failed` (fully reaped), `Forget`, then journaled removal
of the exact verified version directory. The manager never removes files before
that order completes and cannot act on `teddy.manager`.

After lifecycle requests the manager observes launcher `List` responses, up to
three bounded polls; it does not assume that a request is instantaneous.
Launcher events are advisory until the correlated response arrives. A failed
enable/disable, an unsettled state after the bound, an invalid response, or a
rejected request reports failure and stops. Removal may proceed on disabled
`Failed` because that state is fully reaped; other failed actions stop. If
disable succeeds but manager storage is unavailable or busy, removal is not
attempted.

### Explicit update

For an enabled installed plugin, the action list offers `Update` only when the
local catalog supplies a different exact commit. The confirmation shows the
old and candidate commits. The old target must be the exact manager-installed
payload: enabled launcher state, canonical manager payload path, matching
receipt, and matching SHA-256 digest. Disabled, external, ambiguous,
mismatched, self, and same-commit targets are refused.

After confirmation, constrained Git installs and validates the candidate
side-by-side. A durable update journal stage is written before the old record is
disabled, bounded-polled until absent or disabled `Failed` (fully reaped), and
forgotten. The candidate is then enabled and bounded-polled. Candidate startup
failure or timeout triggers rollback by enabling the exact old payload. Both
immutable payloads are retained; old payloads are never deleted.

If a transaction-stage write or compensation cannot be completed safely, the
pending update journal is retained and the manager reports failure. On startup
and refresh, pending journals are validated and reconciled conservatively from
correlated launcher `List` state; launcher events alone do not advance an
update. Updates are explicit, never automatic, and do not build, run hooks or
dependencies, sandbox, verify signatures, or self-update.

The manager lock prevents concurrent manager mutations. Installs and removals
write a bounded journal, sync affected directories for durability, and recover
only validated manager-owned stale staging/trash directories. Promotion is an
atomic same-root rename into an immutable `id/commit` installation. Existing
versions are reused only when the receipt, executable path, and SHA-256 payload
digest match; mismatches or ambiguous versions are refused. Catalog, installed,
or core views report unavailable/truncated data instead of treating incomplete
data as authoritative.

## Accepted sources and exclusions

An install accepts only a canonical public GitHub HTTPS URL ending in `.git`,
an exact 40-character lowercase hexadecimal commit, a `teddy-plugin.v1`
manifest, and a prebuilt executable selected by platform. Every fixed Git
command has a 30-second timeout; each stdout/stderr stream is capped at 8 MiB.
The tree is capped at 16,384 records; each distinct blob is capped at 64 MiB,
and the aggregate materialized blob bytes across tree records at 256 MiB.
SSH, local URLs, other hosts, remote catalogs, builds, hooks, dependencies,
submodules, Git LFS, automatic updates, shell access, and arbitrary commands
are excluded. The manager is not self-manageable and has no sandbox, signature,
or self-update behavior.

Manager config/catalog, immutable side-by-side payload data, and manager lock
state use the XDG paths documented in [configuration](config-spec.md). They are
separate from the core registry at `XDG_CONFIG_HOME/teddy/plugins.bin`.
