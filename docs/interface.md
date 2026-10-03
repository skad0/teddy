# User-facing interface and commands

This file documents the interface implemented in `src/main.rs`.

## Usage and modes

Usage: `teddy [-w workspace-root] [-F|--follow] [file ...]`.

Modes include Edit, ConfirmQuit, Prompt, ReplaceConfirm, Palette, Picker, and
PluginWidget.

Layout uses fixed slots. The bottom slot sits under the editor rows and above
the statusline, and takes about a third of the editor area (3 to 12 rows,
including its title row). It shows the Ctrl+T explorer while that is focused,
and otherwise the jobs pane when it is open.

* **Ctrl+T** focuses the plugin-provided explorer/action surface (a widget
  flagged explorer). If no plugin provides one, the statusline says `no
  explorer plugin`. Esc or Ctrl+T returns to the editor.
* **Jobs pane**: a core log showing plugin failure reasons, exit statuses, and
  stderr tails, plus save, search, and replace outcomes. Its title row shows
  live search, replace, and index progress. A plugin failure opens it, and the
  `jobs` palette command toggles it.

## Palette commands

* `goto <line>`
* `open <path>`
* `tab <n|next|prev>`
* `save`
* `save-as <path>`
* `reload`
* `follow`
* `jobs`
* `quit`

There is no user-facing `plugin-restart` command. Plugin lifecycle control is
manager-only through the bounded launcher protocol.

Plugins may register commands that appear in the palette. Entering such a
command sends `COMMAND_INVOKE` to that plugin with the typed argument. See the
[plugin contract](plugin-contract.md).

## Plugin manager setup

Set `TEDDY_PLUGIN_MANAGER` to an existing absolute executable to opt into the
separately launched manager slot. The bundled manager is not started otherwise.
The core never downloads, bootstraps, builds, updates, or deletes manager
payloads. It remains only the launcher and registry; Git and package work stay
outside the core. The manager cannot control its own `teddy.manager` slot.

The registry starts enabled records before nonpersistent legacy sources. The
bundled highlighter remains automatic and retains its existing precedence.
See [configuration](config-spec.md) for paths and recovery behavior.

## Plugin manager inventory and actions

Set `TEDDY_PLUGIN_MANAGER` to the bundled `teddy-manager` executable (using its
absolute path) to opt in. The manager reads a local catalog, installed receipts,
and core launcher state; it does not use remote catalogs or automatically
update anything.

The manager registers these palette commands:

* `plugins-catalog [query]` — browse the local catalog
* `plugins-installed [query]` — browse installed receipts
* `plugins-core [query]` — browse core launcher state
* `plugins-refresh` — refresh all three inventories

Queries are case-insensitive local substring filters. Selecting an item opens
an action list. Every install, install-and-enable, enable, disable, and remove
action first shows `Cancel` and a separate confirmation choice; selecting
`Cancel` changes nothing. Install downloads the pinned repository content and
leaves the plugin disabled. Install-and-enable installs it, then enables it.
Remove disables first, then polls until it is absent or disabled in `Failed`
(fully reaped), forgets it,
then journaledly removes the exact receipt- and SHA-256-verified immutable local
`id/commit` directory. Launcher events are advisory until the correlated
response arrives; the manager does not treat them as immediate completion.

When an installed plugin is enabled and the local catalog has a different exact
commit, the action list also offers explicit, confirmation-gated `Update`.
Update installs the candidate side-by-side with constrained Git, records a
durable transaction stage, disables/reaps/forgets the old launcher record, then
enables and polls the candidate. Candidate failure or bounded timeout triggers
the exact verified old payload to be enabled again. Both immutable payloads are
retained; update never deletes the old version and is not automatic.

After an enable or disable request, and between disable/forget/remove, the
manager polls the launcher state (up to three bounded list observations).
Running, stopped, absent, and failed states are observed rather than assumed
from the request response. A failed enable/disable or unsettled state stops the
flow and reports it; disabled `Failed` is accepted for removal because it is
fully reaped. Remove never deletes files before the disable/reap/forget order
is complete. Update refuses disabled, external, ambiguous, mismatched, same-
commit, or self targets. Failed transaction-stage writes or failed compensation
retain the pending journal and report safe failure. Startup and refresh
reconcile pending updates conservatively from correlated launcher `List` state.

## Keybindings

* Ctrl-F — Find
* Ctrl-P — Palette
* Ctrl-O — Picker
* Ctrl-G — repeat search
* Ctrl-R — Replace
* Ctrl-Q — Quit, prompting for unsaved changes
* Ctrl-S — Save
* Ctrl-Z / Ctrl-Y — Undo / Redo
* Enter, Tab, character keys — insert
* Backspace / Delete — delete
* Arrow keys, PageUp, PageDown, Home, End — movement

The editor preserves terminal state on panic. Follow mode makes the buffer
read-only and follows the file end. Large-file and search behavior remain as
described by the existing editor implementation.
