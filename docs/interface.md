# User-facing interface and commands

This file documents the main user-visible commands, keybindings and UI flows implemented in src/main.rs.

Usage
- USAGE: `teddy [-w workspace-root] [-F|--follow] [file ...]`

Modes
- Edit — normal editing.
- ConfirmQuit — confirm when unsaved files exist.
- Prompt — status-line text prompt used for Find / Replace flows.
- ReplaceConfirm — interactive replace confirmation flow.
- Palette — Ctrl+P command palette (typed args, suggestion list).
- Picker — Ctrl+O lazy file tree picker.
- PluginWidget — structured list provided by a plugin.

Palette commands (entered via Ctrl+P)
- goto — "goto <line>"
- open — "open <path>"
- tab — "tab <n|next|prev>"
- save — "save"
- save-as — "save-as <path>"
- reload — "reload from disk"
- follow — "toggle follow mode"
- plugin-restart — "plugin-restart <name>"
- quit — "quit"

Keybindings (high-level)
- Ctrl-F — Find (statusline prompt)
- Ctrl-P — Palette (command input)
- Ctrl-O — Picker (file open UI)
- Ctrl-G — repeat / next search
- Ctrl-R — Replace (starts a needle prompt, then replacement)
- Ctrl-Q — Quit (prompts if unsaved changes)
- Ctrl-S — Save (if file changed on disk, second Ctrl-S forces overwrite)
- Ctrl-Z — Undo (group undo)
- Ctrl-Y — Redo
- Enter, Tab, char keys — insert
- Backspace / Delete — delete
- Arrow keys / Shift+Arrows — movement and selection
- PageUp / PageDown / Home / End — movement

Picker details
- Ctrl-O opens a lazy file tree filter (incremental search).
- Select with Enter.

Plugins & commands
- Plugins can register commands that appear in the palette.
- From the palette, entering a registered plugin command sends a COMMAND_INVOKE frame to the plugin with the typed arguments.

Search & replace
- Ctrl-F starts a find prompt. Finds are executed cooperatively and can be canceled by manual keys.
- Ctrl-R starts a replace flow:
  - Enter needle
  - Enter replacement
  - Editor enters ReplaceConfirm mode and steps through matches, allowing y (yes), n (next), a (all), Esc to abort.

Follow mode
- When a buffer is opened with -F or follow turned on, the buffer becomes read-only and the cursor follows to file end.

Status line
- Left and right status areas show buffer name, modified flag, readonly, binary/hugeness/no-index status and location (Ln/Col or byte position).

Notes for UX
- Large files use a "huge" mode with byte-windowed rendering and some operations disabled (replace-all disabled).
- The editor preserves terminal state on panic (it restores terminal).
