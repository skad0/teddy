# Configuration

## Core plugin registry

The core-owned registry is stored at
`$XDG_CONFIG_HOME/teddy/plugins.bin`, or `$HOME/.config/teddy/plugins.bin`
when `XDG_CONFIG_HOME` is unset. It is a bounded, versioned, opaque binary;
users and managers must not edit it directly. Use the launcher protocol
described in [`plugin-contract.md`](plugin-contract.md).

Each record contains a stable validated ID, an absolute executable path,
desired `enabled` state, a bounded restart policy (`max_restarts` and
`backoff_ms`), and generation information. Desired state is distinct from the
observed lifecycle of its process.

A missing registry is an empty registry. Corrupt input is loaded in recoverable
mode, reported through the editor status surface, and refuses ordinary
mutations. A manager must send a descriptor `Enable` with explicit recovery
confirmation; that recovery save is accepted once, after which normal saves
again use generation conflict checking. Atomic replacement and locking protect
successful writes. A commit accompanied by a directory durability warning is
still committed and is not retried; the warning is surfaced separately. Stale
concurrent instances receive a generation conflict rather than overwriting
newer state.

The core is a runtime launcher and registry, not a package manager. It does no
Git or package work. The separately launched bundled `teddy-manager` performs
the documented local manager operations only when `TEDDY_PLUGIN_MANAGER` is an
existing absolute executable. The core never receives shell, argv, Git, or
package authority.

## Runtime sources

* `TEDDY_PLUGINS` is a nonpersistent colon/semicolon-separated list of plugin
  executable paths. Its relative order is preserved.
* The bundled sibling `teddy-highlight` is added automatically when available
  and not already configured. A configured highlighter retains precedence.
* The bundled sibling `teddy-ai` is not added automatically. Put its absolute
  path in `TEDDY_PLUGINS` to run its framed `ai` shell directly.
* `TEDDY_PLUGIN_MANAGER` is opt-in and must be an existing absolute regular
  executable with execute permission. It is not downloaded or bootstrapped and
  runs in the reserved `teddy.manager` slot.

The other reserved IDs are `teddy.bundled.highlight` and deterministic
`teddy.legacy.*` IDs. Registry and manager-created IDs may not use `teddy.*`.
Only `teddy.manager` may send launcher requests, and it cannot control itself.

## Environment variables

* `TEDDY_PLUGINS` — nonpersistent plugin path list.
* `TEDDY_PLUGIN_MANAGER` — optional absolute manager executable.
* `TEDDY_PERF` — performance log path when built with the `perf` feature.

There are no AI provider settings: no provider, API-key, environment, CLI,
model, or other AI configuration is implemented.

The manager's files are separate from the core registry:

* manager config: `$XDG_CONFIG_HOME/teddy/plugin-manager`, or
  `$HOME/.config/teddy/plugin-manager`;
* catalog: `.../plugin-manager/catalog`;
* manager data: `$XDG_DATA_HOME/teddy/plugins`, or
  `$HOME/.local/share/teddy/plugins`;
* manager state and lock: `$XDG_STATE_HOME/teddy/plugin-manager`, or
  `$HOME/.local/state/teddy/plugin-manager`.

The manager data contains immutable side-by-side `id/commit` installations;
the core registry remains the separate `plugins.bin` file above. Manager
actions mutate desired launcher state through the launcher protocol, but
observed state is obtained by bounded polling and is not assumed instantaneous.
Manager install/removal journals and directory durability syncs are kept under
the manager state area; stale staging/trash recovery is bounded and refuses
unknown artifacts. Payload reuse and removal require exact receipt, digest, and
payload-path matches. A Git command is limited to 30 seconds and each output
stream to 8 MiB; tree/blob validation is capped as described in
[the manifest reference](plugin-manifest.md).
The state directory also contains the pending update journal. The manager is
the only component that orchestrates these operations; the core only applies
launcher/registry requests.
