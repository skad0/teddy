# Plugin recipes

This page gives practical ways to use the bundled manager. The manager must
already be built and its path must be absolute:

```sh
export TEDDY_PLUGIN_MANAGER="$PWD/target/release/teddy-manager"
teddy
```

The core does not download or bootstrap `teddy-manager`. It remains the
launcher and registry; Git and package work belong to the manager.

## Catalog workflow

The manager reads the local catalog at:

```text
$XDG_CONFIG_HOME/teddy/plugin-manager/catalog
```

or `$HOME/.config/teddy/plugin-manager/catalog` when `XDG_CONFIG_HOME` is
unset. The catalog is a reviewed set of pinned `teddy-catalog.v1` entries; it
is not fetched remotely. Use Ctrl-P for:

* `plugins-catalog [query]` — browse and filter catalog IDs;
* `plugins-installed [query]` — inspect receipt-backed installations;
* `plugins-core [query]` — inspect launcher desired and observed state;
* `plugins-refresh` — reload the three views.

Select a catalog ID, then choose an action and confirm it. Every action has a
cancel-first confirmation. Install leaves the plugin disabled; Install and
enable installs the pinned prebuilt executable and then enables it. Enable and
Disable operate on the exact installed payload. Remove disables, waits for
absence or disabled `Failed` (fully reaped), forgets the launcher record, then
removes the verified payload. Lifecycle completion is based on correlated
responses and bounded `List` polling, not advisory events.

For an enabled exact installed payload, a catalog entry with a different exact
commit exposes explicit `Update`. The manager installs the candidate
side-by-side, journals durable stages, disables/reaps/forgets the old record,
then enables and polls the candidate. Candidate failure or timeout rolls back
to the exact old payload. Both immutable payloads remain.

## Common bundles

A “common bundle” here means a curated **recipe set**: a list of independently
cataloged plugin IDs selected for a task. There is no bundle primitive,
transaction, dependency resolver, or bundled-manager command. To use a recipe
set, obtain the reviewed local catalog, search for each listed ID, inspect its
pinned repository and platform executable, and install or install-and-enable
each entry separately. If one fails, the other actions are not implicitly
rolled back.

Do not install the bundled `teddy-highlight` as a catalog plugin. The core adds
the bundled highlighter separately when available and preserves its configured
precedence.

## Receipts, recovery, and troubleshooting

Manager data is under `$XDG_DATA_HOME/teddy/plugins` (or
`$HOME/.local/share/teddy/plugins`) as immutable `id/commit` directories.
Manager state, lock, install/removal journals, and pending update journals are
under `$XDG_STATE_HOME/teddy/plugin-manager` (or
`$HOME/.local/state/teddy/plugin-manager`). Receipts bind the ID, commit,
payload path, and SHA-256 digest; mismatches, ambiguous versions, external
paths, and unsafe artifacts are refused.

If a catalog is unavailable, run `plugins-refresh` after placing the reviewed
catalog at the local path. If the manager is busy, retry after the other
manager operation exits. If an install, stage write, compensation, or removal
reports safe failure, do not delete staging, trash, receipts, or journals by
hand; restart teddy and refresh so bounded manager recovery and pending-journal
reconciliation can inspect correlated launcher `List` state. A failed update
may leave both payloads and its pending journal intentionally.

There is no remote catalog, automatic update, build, hook, dependency install,
shell access, manager bootstrap, sandbox, signature verification, or manager
self-update. Updates are explicit and confirmation-gated.
