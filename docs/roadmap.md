# teddy — future improvements & UI plan

Status: accepted (Seq 55, 2026-10-03); see §6 for the user decisions.
Checked against `docs/raw_spec.md` (locked; §21 rejection list is law) and
deduplicated against To Do / running tasks Seq 42–53.

## 1. Where teddy stands

The engine is done and measured: S0–S8 landed, 1 GiB open in ~6 ms, streaming
search 3.5× the nearest editor (`docs/bench_comparison.md`). The **editing
surface is the weak side**. A user opening teddy today finds no copy/paste, no
way to close a tab, no word movement, a statusline hint and nothing else for
discoverability. The board (42–53) already covers terminal input, Unicode
width, config, block selection, widgets and the plugin bodies. What is left
uncovered is mostly **basic editing ergonomics and a few spec-mandated items
that silently never landed**.

Verified gaps (each checked in source, not inferred):

| Gap | Evidence | Spec |
|---|---|---|
| No copy / cut / paste (no register, no OSC 52, Ctrl+C/X/V unbound) | key match `src/main.rs:1648-1772` binds only F P O G R Q S Z Y; zero hits for `clipboard`/`OSC`/`52;` in `src/` | §1 "core must be usable alone: … edit" |
| No close-tab / new-buffer command | builtin palette list `src/main.rs:629-636` | §1 "tabs" |
| Tabline drops tabs past the width; active tab can be invisible; same-basename tabs indistinguishable | `src/render.rs:318-320` (`break` on overflow), label is `name` only | §1 "explicit visible tabs only" |
| No word / document-edge movement | `Key` enum `src/input.rs:6-28` has no Ctrl-modified arrows/Home/End | — (new) |
| Huge/unindexed `goto` refuses instead of byte/percent | `src/main.rs:1383-1384` "no line index yet" | §6 "goto: byte offset / percent" — **spec-mandated, unimplemented** |
| Crash path does not write a crash log | panic hook `src/main.rs:139-143` restores + default report only | §20 "write crash log, print path" — **spec-mandated, unimplemented** |
| Huge-file save is always a full synchronous temp rewrite | `src/buffer.rs:479-556`, no `pwrite`, runs inline in the loop | §9.2 adaptive save, §18 "save streaming" incremental — **spec-mandated, unimplemented** |
| Undo groups never break on a typing pause | grouping is `group_counter`/`last_edit_kind` only, no time input (`src/main.rs:656-668`) | §8 "break on pause" — **spec-mandated, unimplemented** |
| Focus events not requested/parsed | zero hits for `1004` in `src/` | §12 core owns focus events — **spec-mandated, unimplemented** (not in Seq 45's brief) |
| Plugin failure only a statusline line | `src/main.rs:802` | §14.4 "details in bottom/job pane" — **spec-mandated, unimplemented** |
| No external search plugin example | `src/bin/` has demo/highlight/ai/lsp/session/manager only | §22 S8 "external search plugin examples" — **spec-mandated, unimplemented** |
| No line-number gutter | zero hits for `gutter`/`line_number` in render/main | — (new, optional) |

## 2. UI / UX direction

The spec fixes the frame: non-modal, Nano-like, Ctrl profile, alternate
screen, tabline always visible, one editable center, fixed left/right/bottom
slots for core/plugin panes, no splits, no floating surfaces. Inside that frame
the direction is **"boring modern editor keys, zero surprises, everything
discoverable from Ctrl+P"**:

1. **Keys match what people already type.** Ctrl+C/X/V/A, Ctrl+W close tab,
   Ctrl+N new buffer, Ctrl+←/→ word, Ctrl+Home/End document edges,
   Shift-variants extend selection. Ctrl+Z stays undo (no job-control suspend);
   Ctrl+T stays reserved for Seq 47.
2. **The palette is the help system.** An empty Ctrl+P already lists commands,
   but paints at most 8 rows and never scrolls (`src/main.rs:1906`), so plugin
   commands past the 8th and a selection moved below row 8 are invisible. Fix
   the window to follow the selection, then extend each row with its key binding and make it the one place to learn
   teddy. A `keys` command opens a **read-only scratch tab** with the
   cheatsheet. No popup overlays — they would violate "fixed slots only".
3. **The statusline tells the truth, quietly.** Keep the flag row (RO BIN HUGE
   NOIDX FOLLOW CHG). Replace the static "Ctrl+Q quit" hint with the
   contextual next action only when relevant (selection active → "Ctrl+C
   copy", search running → "Esc cancel", dirty quit prompt keys). Format
   becomes configurable once Seq 43's config file lands.
4. **Tabs never lie.** Active tab always visible (scroll the strip), `‹ n more ›`
   markers on overflow, `parent/name` disambiguation for duplicate basenames,
   width-correct labels after Seq 44 and escaped names after Seq 42.
5. **The bottom slot becomes the "jobs" pane.** One core pane for plugin
   failures, long-running save/search progress, and later AI/LSP output
   (the logs widget from Seq 47). Same slot, not a new surface.
6. **Huge files feel first-class.** `goto 42%` / `goto @123456789` / `goto +10M`,
   save progress in the jobs pane, cancellable save.
7. **Theme by name.** A theme picked by name from the config file; colors stay
   ANSI/256/truecolor SGR. No JSON parser in core (see §5 open question).

## 3. Ranked proposals

Size: **S** ≤ half a day / <200 LOC, **M** 1–2 days, **L** multi-day.
"Nearest" names the closest Seq 42–53 task and why this is not a duplicate.

| # | Proposal | Kind | Size | Nearest task / dedup | §21 check |
|---|---|---|---|---|---|
| 1 | **Clipboard: copy/cut/paste/select-all** | new (core usability) | M | Seq 45 = terminal *bracketed paste* (input parsing); Seq 46 = block selection, which needs a copy primitive. **Distinct; land before 46.** | §21 OK, but **memory-bound (§5.1/§19)**: the register holds piece refs like undo (§8) or spills via the add store (§5.3), never a raw `Vec` of the selection; OSC 52 (base64) only below a size cap, else skipped with a statusline notice; cut on huge files is a normal delete under the §7 transaction cap. No daemons, no threads. |
| 2 | **Tab lifecycle: close (Ctrl+W, dirty prompt), new buffer (Ctrl+N), tabline overflow + disambiguation** | spec §1 gap | M | Seq 42 touches tabline filename rendering (file contention in `src/render.rs:305-332`): **sequence after 42**. Seq 44 owns width math. | OK |
| 3 | **Huge-file goto: byte offset and percent** | spec §6, unimplemented | S | none (Seq 52 is perf, not navigation) | OK |
| 4 | **Crash log on panic** (write `$XDG_STATE_HOME/teddy/crash-<pid>.log`, print path) | spec §20, unimplemented | S | none | OK |
| 5 | **Word & document-edge movement** (Ctrl+←/→, Ctrl+Home/End, Ctrl+Backspace/Delete, Shift variants) | new | S | Seq 45 = Kitty/CSI-u probing. Plain xterm `CSI 1;5X` sequences are a separate, small parser addition — **if Seq 45 hasn't started, fold into 45**; otherwise standalone. | Word = ASCII class + UTF-8 boundary; no grapheme engine. OK |
| 6 | **Undo grouping breaks on pause** (configurable ms) | spec §8, unimplemented | S | none | OK |
| 7 | **Adaptive huge save: in-place `pwrite` for length-preserving edits + incremental cancellable streaming rewrite** | spec §9.2/§18, unimplemented | L | Seq 52 is a measured perf sweep, not a save-strategy change. **Distinct; correctness-critical — wants Codex adversarial review.** | No mmap; pwrite + step(max_bytes). OK |
| 8 | **Palette as help: scrolling result window (today capped at 8 rows, `src/main.rs:1906`), key hints per row, `keys` read-only cheatsheet tab** | new (discoverability) | S | Seq 47 adds widget kinds; this uses existing palette/buffer. Distinct. | Read-only tab, not a popup or rich preview. OK |
| 9 | **Bottom "jobs" pane in core: plugin failures, save/search progress** | spec §14.4, unimplemented | M | **Fold into Seq 47** if its brief can grow (it builds the logs widget + slot rendering); otherwise do right after 47 using its logs widget. | Core widget, fixed bottom slot. OK |
| 10 | **Contextual statusline hints** | new | S | Seq 43 makes the statusline *configurable*; this changes the *default* content. Do after 43, small. | OK |
| 11 | **Focus events** (`?1004h`; re-stat open files on focus-in, refresh CHG) | spec §12, unimplemented | S | **Fold into Seq 45** (same parser, same mode enable/restore path); flag to the coordinator because 45's brief omits it. | OK |
| 12 | **Theme loading by name** | spec §3/§4 "theme compiler" | S–M | **Fold into Seq 43** (config file); needs the JSON-vs-TOML decision below first. | Tiny TOML subset only. OK |
| 13 | **External search plugin example** (`teddy-grep`: shells to `rg`/`grep`, results in a list widget, open-at-match) | spec §22 S8, unimplemented | M | Seq 47 (widgets) is a dependency for a tree/table view; a list-widget version works today. Distinct. | Regex/project search plugin-owned — allowed outside core. OK |
| 14 | **Optional line-number gutter** (indexed files only; off in huge/NOIDX) | new | S–M | Seq 44 also edits row rendering/column math — **sequence after 44**. | Visible rows only, no global work. OK |
| 15 | **Huge-file explicit "index now" command** (opt-in full newline scan, cancellable) | spec §5.4 "unless explicitly indexed" | M | Seq 52 touches the newline scan (`src/buffer.rs:350`) — **sequence after 52**. | Opt-in, cooperative. OK |
| 16 | **Picker fuzzy ranking** (subsequence score over the lazily walked set) | spec §17 "fuzzy search improves progressively" | S–M | none | No index. OK |

### Recommended order

**Wave A — usable editor (parallel-safe, small):** 4 crash log, 3 huge goto,
6 undo pause. Then 1 clipboard (prerequisite for Seq 46) and 5 word movement
(or folded into 45).
**Wave B — tabs & discoverability:** 2 tab lifecycle (after Seq 42 merges),
8 palette-as-help, 10 statusline hints (after Seq 43).
**Wave C — spec debt with risk:** 7 adaptive huge save (alone; adversarial
review), 9 jobs pane (with/after Seq 47), 13 search plugin example.
**Wave D — polish:** 14 gutter, 15 explicit index, 16 fuzzy picker.

Rationale for the top: items 1–2 are the first things any user hits and today
have no workaround; 3, 4, 6 are spec-mandated, tiny, and contention-free, so
they are the cheapest way to shrink spec debt. Item 7 is the largest remaining
spec gap but has a working (if slow) fallback, so it waits for a calm window.

### Suggested folds into existing briefs (coordinator's call)

- Seq 43 ← theme-by-name (12), statusline format key (prereq for 10).
- Seq 45 ← focus events (11); optionally plain xterm modifier keys (5).
- Seq 47 ← bottom jobs pane for plugin failures (9).

## 4. Considered and rejected (do not re-propose)

| Idea | Why | Cite |
|---|---|---|
| Soft wrap for long lines | rejected from core; Seq 44 bounds long-line rendering instead | §21 "soft wrap" |
| Multi-cursor / multiple selections | rejected | §21 "multi-cursor" |
| Editable split panes / side-by-side | rejected | §1 "no editable split panes" |
| Floating help / hover / completion popups | fixed slots only; completion lists go through LSP widgets | §1 "layout: fixed slots only", §14.3 |
| Minimap | needs whole-file scan + extra surface | §4 hot path, §1 slots |
| Embedded terminal | rejected | §21 "embedded PTY" |
| Regex search in core | rejected; plugin-owned (#13) | §3, §21 |
| Markdown/HTML preview | rejected | §21 |
| Diff/merge UI, git gutter in core | rejected; plugin decorations only | §21 "diff UI" |
| Session restore in core | plugin-owned (Seq 48) | §21 |
| Backup / swap files, trash on delete | rejected | §21 |
| Recursive workspace watcher | rejected | §21 |
| Scripting / WASM config | rejected | §21 |
| Ctrl+Z suspend (SIGTSTP) | Ctrl+Z is undo in the locked key profile | §1 |
| Full grapheme / bidi cursoring | rejected; basic width table only (Seq 44) | §11.3, §21 |

## 5. Open questions for the user

1. **Theme format.** `docs/theme-spec.md` specifies JSON theme manifests, but
   §3 blesses only a tiny TOML subset parser in core and a JSON parser has no
   home there. Recommended: themes become a section of the Seq 43 config file
   (TOML subset) and `theme-spec.md` is rewritten to match.
2. **Clipboard reach.** OSC 52 works over SSH/tmux (with `set-clipboard on`)
   but cannot *read* reliably. Recommended: write via OSC 52, paste from the
   internal register; system paste arrives as bracketed paste (Seq 45).
3. **Ctrl+W.** Close-tab (browser/modern) vs. nano's "where is". teddy's
   search is Ctrl+F, so Ctrl+W = close tab is recommended.

## 6. User decisions (2026-10-03)

- **Open questions (§5): all accepted as recommended.** Themes live in the
  Seq 43 config file (TOML subset) and `theme-spec.md` is to be rewritten to
  match. The clipboard writes through OSC 52 and pastes from the internal
  register. Ctrl+W closes the tab.
- **Folds into existing tasks:** Seq 43, Seq 45 and Seq 47, per §3
  "Suggested folds into existing briefs".
- **Other proposals:** filed as board tasks Seq 56–67. The board is the source
  of truth for which proposal maps to which task.
