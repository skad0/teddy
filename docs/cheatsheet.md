# teddy cheatsheet

## Launch

```sh
cargo build --release
./target/release/teddy [file ...]

# With an existing absolute manager executable
export TEDDY_PLUGIN_MANAGER="$PWD/target/release/teddy-manager"
./target/release/teddy [file ...]

# Run the bundled teddy-ai sibling directly as a nonpersistent plugin
export TEDDY_PLUGINS="$PWD/target/release/teddy-ai"
./target/release/teddy
```

Use an absolute path for `TEDDY_PLUGIN_MANAGER` and for entries in
`TEDDY_PLUGINS`. `teddy-ai` is shipped by the `teddytor` package as an implicit
Cargo binary from `src/bin/teddy-ai.rs`; it is not a separate crate or catalog
manager package.

## Keys

| Key | Action |
| --- | --- |
| Ctrl-F | Find |
| Ctrl-P | Command palette |
| Ctrl-O | Tree/file picker |
| Ctrl-T | Plugin explorer/action pane (when a plugin provides one) |
| Ctrl-G | Repeat search |
| Ctrl-R | Replace |
| Ctrl-Q | Quit (confirm if unsaved) |
| Ctrl-S | Save |
| Ctrl-Z / Ctrl-Y | Undo / redo |
| Enter, Tab, character keys | Insert |
| Backspace / Delete | Delete |
| Arrow keys, PageUp, PageDown, Home, End | Move |

## Palette

Press Ctrl-P and type one of:

```text
goto <line>
open <path>
tab <n|next|prev>
save
save-as <path>
reload
follow
jobs
quit
```

When a manager is enabled, it also offers:

```text
plugins-catalog [query]
plugins-installed [query]
plugins-core [query]
plugins-refresh
```

Select a catalog or installed item to use its actions: install, install and
enable, enable, disable, remove, or (for an eligible enabled plugin at a
different exact catalog commit) update. Actions are confirmation-gated;
install leaves a plugin disabled. These commands operate on manager-installed
catalog plugins, not on bundled sibling executables.

## `teddy-ai` status

The direct `TEDDY_PLUGINS` example exercises the shipped out-of-process framed
plugin shell and its registered `ai` command. Invoking `ai` currently reports
`ai shell: no provider configured`. It has no provider, API key, environment
setting, CLI arguments, model/configuration, or functional AI behavior yet.
It is not a functional AI bundle and is not installed through the manager
catalog; running it directly is only useful for exercising its shell/protocol.
