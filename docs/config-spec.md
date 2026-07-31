# Configuration spec (suggested)

teddy currently reads a small set of runtime environment variables. There is no built-in config file format present in the repo; the following is a recommended config spec (YAML) for a simple, flexible configuration file should you add persistent config support.

Environment variables already used
- TEDDY_PLUGINS — colon/semicolon separated list of executable plugin paths. Editor will spawn these at startup.
- TEDDY_PERF — (used if compiled with `--features perf`) path to write perf logs.

Recommended config file: ~/.config/teddy/config.yaml (YAML)
```yaml
# Example ~/.config/teddy/config.yaml
editor:
  default_workspace: ~/projects
  follow_on_open: false     # default follow behavior
  open_files:
    - src/main.rs

plugins:
  # absolute or workspace-relative path entries
  - path: /usr/local/lib/teddy-plugins/gitplugins/teddy-git
    autostart: true
  - path: ./tools/teddy-lsp
    autostart: false

ui:
  theme: default
  show_raw_bytes_for_binary: false

keybindings:
  save: "Ctrl+S"
  quit: "Ctrl+Q"
  palette: "Ctrl+P"
  find: "Ctrl+F"
  replace: "Ctrl+R"

logging:
  level: info
  perf_path: null
```

Config semantics
- editor.default_workspace — path used when opening a workspace (if -w not given on CLI).
- plugins[*].autostart — if false, plugin is available but not started automatically; the editor could expose a command to start it.
- ui.theme — theme name mapping to a theme manifest (see docs/theme-spec.md).
- keybindings — optional override for default keymap (requires editor support to make it configurable).

Notes
- This is a suggested schema for adding persistent configuration; the codebase currently reads TEDDY_PLUGINS and TEDDY_PERF. If you want I can add a small parser to load this YAML on startup.
