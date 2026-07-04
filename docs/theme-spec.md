# Theme spec (terminal)

teddy exposes a simple theme mapping used for plugin-provided span styles and internal UI components. Because teddy is a terminal editor, themes map named style indices to ANSI SGR attributes (foreground, background, bold/italic/underline).

Theme manifest (JSON)
- A theme is a JSON file with:
```json
{
  "name": "default",
  "variables": {
    "bg": "ansi:0",
    "fg": "ansi:15",
    "accent": "ansi:4"
  },
  "styles": {
    "0": {"fg": "fg", "bg": null, "attrs": []},
    "1": {"fg": "ansi:2", "bg": null, "attrs": ["bold"]},
    "2": {"fg": "ansi:1", "bg": null, "attrs": []}
  },
  "span_index_map": {
    "0": "normal",
    "1": "keyword",
    "2": "string"
  }
}
```

Fields
- name: human name of the theme.
- variables: named color variables; values are either:
  - "ansi:N" where N is an integer 0..15,
  - a 24-bit RGB hex "#RRGGBB" (terminal must support truecolor), or
  - a variable reference like "fg".
- styles: mapping from numeric style id (stringified integer) to:
  - fg: either variable name or direct color spec,
  - bg: variable name or null,
  - attrs: array of strings e.g. ["bold","underline","reverse"]
- span_index_map: optional mapping from numeric plugin style indexes (u8 style values in SPANS payload) to logical style names in `styles`. Example: plugin emits style=2 for strings and theme maps "2" -> "string" -> styles["2"].

Editor mapping details
- Plugins send SPANS with a u8 style value; theme.span_index_map maps that numeric style value to a theme style entry.
- If a plugin's style value is unmapped, editor falls back to style "0".
- Widgets and status line elements use theme variables for foreground/background.

Example mapping (simple)
- style 0 -> default text
- style 1 -> keyword (bold, blue)
- style 2 -> string (green)
- style 3 -> comment (dim, italic)

Implementation notes
- Terminal attribute mapping: map style attributes to ANSI SGR codes.
- Respect terminal color capabilities. If truecolor supported, prefer hex; otherwise map to nearest 16/256-color value.
- Provide a `--list-themes` or `--theme` CLI option eventually to load theme manifests.

This theme spec is intentionally simple and designed for terminal use and for being mapped to the small numeric style values used in the plugin SPANS payloads.
