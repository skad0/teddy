# Phase 2 — Ruthless Engine Validation & Architecture

## Final decision

Build a **custom Unix-native Rust core**.

Do **not** build on Ropey, Crossterm, Ratatui, OpenTUI, or Qem as the editor engine. Use selected ideas, not their abstractions.

The core must be:

```text
non-modal
Unix-native macOS/Linux
single-threaded poll loop
direct ANSI renderer
pread-backed chunk cache
byte-range edit transactions
chunked piece-chain buffer
out-of-process plugins only
no mmap
no async runtime
no terminal UI framework
no parser/LSP/scripting/runtime in core
```

The hardest constraint is not “large file support.” It is this combination:

```text
multi-GB files
sub-ms input/render latency
byte-preserving invalid UTF-8 behavior
minimal RSS
tiny binary
deep customization
out-of-process plugins
single-threaded core
```

That rules out most convenient Rust editor/TUI stacks.

---

# 1. Locked UX / product contract

The editor is a **non-modal Nano-like terminal editor**, not Vim/Helix/Emacs-like. Default keys use a modern Ctrl profile:

```text
Ctrl+S  save
Ctrl+Q  quit
Ctrl+F  search
Ctrl+O  file picker
Ctrl+P  command palette
Ctrl+T  explorer/action surface if enabled
```

Core UX rules:

```text
tabs: explicit visible tabs only
tabline: always visible
statusline: configurable, minimal by default
layout: fixed slots only
center: one editable editor area
left/right/bottom: plugin/core panes only
no editable split panes
no embedded terminal/PTTY
alternate screen always
mouse: opt-in; full mouse UI except drag-and-drop
```

Core must be usable alone:

```text
open/save/edit/search/replace
tabs
command palette
lazy palette tree picker
basic file operations
literal search/replace
session undo/redo
```

Plugins are enhancement, not survival.

---

# 2. Engine showdown

## 2.1 Custom engine vs ecosystem

### Verdict

Use a **custom text engine and custom renderer**.

The ecosystem crates are rejected from the core when they violate any of:

```text
loads or represents whole file as in-memory text
requires valid UTF-8 everywhere
forces char/line indexing as primary addressing
pulls broad cross-platform terminal abstraction
owns rendering buffers/widgets/event loop
uses mmap when the locked design says pread chunk cache
creates hidden allocation paths in hot loops
```

The core should use only tiny curated crates when they reduce correctness risk more than they cost in binary size, memory, and latency.

---

## 2.2 Qem

Qem is close in theme: it presents itself as a Rust text engine for very large documents with file-backed reads, incremental line indexing, and responsive editing; its docs describe mmap-backed access, sparse on-disk line indexes, and mutable rope/piece-table edit buffers. ([docs.rs][1])

### Reject as core dependency

Reasons:

```text
1. Locked design says no mmap.
2. Locked design says original backing = pread chunk cache only.
3. Huge files must avoid global line indexing by default.
4. Core edit transaction model is byte-range replacement only.
5. We need exact control over cache policy, watcher interaction, dirty reload, spill files, undo caps, and degraded modes.
```

Qem is useful as a comparison point, not as the engine. Its mmap-centric design conflicts with the user-locked storage model.

### Cost if used

```text
Bundle: unknown/avoidable dependency weight.
Memory: risk from indexes/caches not governed by our caps.
Performance: good for its model, but wrong fault model because mmap page faults are not scheduled by our cooperative event loop.
Decision: reject.
```

---

## 2.3 Ropey

Ropey is an editable UTF-8 rope designed as a text-buffer backing structure for editors. Its docs state that its atomic unit is Unicode scalar values / Rust `char`s, and that editing/slicing is done by char indices to prevent accidental invalid UTF-8 creation. ([docs.rs][2])

### Reject as core buffer

Reasons:

```text
1. The editor is byte-addressed.
2. Invalid UTF-8 must be preserved and rendered as escaped bytes.
3. Multi-GB files must not be loaded into an in-memory rope.
4. Huge-file mode must work without global line/char indexes.
5. LSP/plugin adapters must convert into byte-range transactions, not force char-indexed core semantics.
```

Ropey is a good general-purpose Rust rope. It is the wrong primitive for this editor.

### Cost if used

```text
Bundle: acceptable in many apps, but unnecessary here.
Memory: in-memory tree representation conflicts with pread-backed sparse storage.
Performance: good for normal files; wrong for byte-preserving huge files.
Decision: reject.
```

---

## 2.4 Crossterm

Crossterm is a pure-Rust cross-platform terminal manipulation library supporting Unix and Windows terminals. ([docs.rs][3])

### Reject as core terminal backend

Reasons:

```text
1. Target is Unix-native macOS/Linux, not Windows.
2. Rendering must be direct ANSI into one reusable frame buffer.
3. Input parser, mouse handling, enhanced keyboard probing, and dirty-region rendering must be core-owned.
4. Broad terminal abstractions hide allocation and command-emission behavior.
5. Cross-platform convenience is lower priority than tiny binary and deterministic hot paths.
```

### Cost if used

```text
Bundle: higher than direct Unix layer.
Memory: likely low, but unnecessary abstraction state.
Performance: probably fine for many TUIs, but less auditable for sub-ms hot path.
Decision: reject from core.
```

---

## 2.5 OpenTUI

OpenTUI’s Rust crate describes itself as a high-performance terminal UI rendering engine and a Rust port of the OpenTUI Zig core, giving control over buffers, cells, colors, and text without a prescribed widget tree or event loop. ([docs.rs][4]) The OpenTUI project itself describes a native Zig core with a C ABI and component-oriented terminal UI capabilities. ([opentui.com][5])

### Reject as core renderer

OpenTUI is closer than Crossterm or Ratatui, but still wrong for this locked design.

Reasons:

```text
1. Core rendering must be a bespoke row/region diff into one reusable ANSI buffer.
2. Plugin views are core widgets only, not arbitrary cell surfaces.
3. We do not want a general cell-buffer renderer as the central abstraction.
4. The editor must optimize around text viewport rows, byte ranges, selections, and dirty row bits.
5. Full terminal cell-grid abstraction risks extra retained memory and unnecessary composition layers.
```

### Cost if used

```text
Bundle: more than a direct renderer.
Memory: likely cell buffers and renderer state beyond our minimum.
Performance: high-performance generally, but not specialized enough for this text-editor hot path.
Decision: reject from core.
```

---

## 2.6 Ratatui-style UI frameworks

Ratatui’s docs describe widgets as building blocks for terminal interfaces that can be combined and nested for complex UIs. ([docs.rs][6])

### Reject

This editor explicitly rejects a general widget/layout framework in the core. The core has fixed layout slots, a small internal widget set, and a text renderer specialized for visible editor rows.

```text
Bundle: too high for core target.
Memory: widget/layout state not justified.
Performance: likely acceptable for dashboards, wrong abstraction for huge-file editor core.
Decision: reject.
```

---

## 2.7 In-process dynamic plugin crates

Dynamic library loading crates exist, and `libloading` provides safer bindings around platform dynamic loading primitives. ([docs.rs][7]) Rust-to-Rust dynamic plugin ABIs are not simple: the Rust Reference states that the Rust ABI has no stability guarantees, while `extern "C"` maps to the platform C ABI. ([GitHub][8]) `abi_stable` exists specifically to provide FFI-safe types and trait-object-like mechanisms for stable plugin boundaries. ([docs.rs][9])

### Reject public in-process plugins

The locked design uses **out-of-process plugins only**.

Reasons:

```text
1. No plugin may crash the editor process.
2. No plugin heap may become core RSS.
3. No Rust ABI/version trap.
4. No plugin code in typing/render hot path.
5. AI/model plugins are inherently async/slow and belong outside core.
```

---

# 3. Accepted low-level building blocks

The core may use tiny curated crates only where they avoid dangerous custom Unix code.

Acceptable categories:

```text
termios/raw mode wrapper
signal handling wrapper
minimal libc/rustix-style Unix syscall access
tiny Unicode width table/crate
tiny TOML parser or narrow hand-written TOML subset
small argument parser or hand-written CLI
```

Rejected categories:

```text
async runtime
thread pool
terminal UI framework
cross-platform terminal abstraction
rope/text-buffer engine
regex engine in core
parser framework
tree-sitter in core
LSP in core
scripting runtime
JSON IPC in production core
plugin package manager
```

Dependency rule:

```text
Every core dependency needs a binary-size, RSS, and hot-path allocation audit.
Default feature flags are treated as hostile until inspected.
```

---

# 4. Core architecture

```text
┌────────────────────────────────────────────────────────────┐
│                         Core                               │
├────────────────────────────────────────────────────────────┤
│ CLI / config / theme compiler                              │
│ Unix terminal backend: raw mode, input parser, ANSI writer  │
│ Single-threaded poll loop                                  │
│ Buffer manager / tabs                                      │
│ pread-backed original-file cache                           │
│ Chunked piece-chain edit engine                            │
│ Undo/redo transaction log                                  │
│ Viewport mapper / render cache                             │
│ Dirty-region renderer                                      │
│ Literal search / replace                                   │
│ File picker: lazy CWD/workspace tree                       │
│ File watcher: inotify/kqueue                               │
│ Plugin host: stdio binary protocol                         │
└────────────────────────────────────────────────────────────┘

External processes:
  AI plugins
  LSP plugin
  highlighter plugins
  session/recovery plugin
  explorer/search plugins
  versioning/checkpoint plugins
```

The hot path is:

```text
read input
decode key/mouse
apply small edit/navigation
mark buffer/view rows dirty
coalesce pending events
render dirty regions into one reusable frame buffer
write once
```

Nothing in that path may:

```text
scan whole file
parse syntax globally
call plugin synchronously
allocate per cell
allocate per keypress
block on disk except already-hot small reads
block on network
run hooks
run shell commands
```

---

# 5. File and buffer memory model

## 5.1 Original file source

Original file bytes are not loaded into RAM.

Locked model:

```text
original file backing = pread chunk cache only
no mmap
```

`pread` reads from a file descriptor at a specified offset without changing the file offset, which fits a byte-window cache and avoids shared seek state. ([man7][10])

### Structure

```rust
struct OriginalFile {
    fd: RawFd,
    path: PathBuf,
    len: u64,
    mtime: Timespec,
    size_at_load: u64,
    newline_style: NewlineStyle,
    readonly: bool,
    binary_class: BinaryClass,
    chunk_cache: OriginalChunkCache,
}
```

Chunk cache:

```rust
struct OriginalChunkCache {
    chunk_size: u32,              // default 64–128 KiB
    viewport_prev: Option<Chunk>,
    viewport_current: SmallVec<ChunkRef>,
    viewport_next: Option<Chunk>,
    lru: FixedLru<ChunkKey, ChunkBuf>,
}
```

Policy:

```text
default chunk size: configurable 64–128 KiB
prefetch: bidirectional around viewport
random/search cache: small bounded LRU
eviction: explicit cap
```

### Cost

```text
Bundle: small; direct Unix positioned I/O.
Memory: bounded by chunk count × chunk size.
Performance: predictable; avoids mmap page-fault surprise and mmap invalidation edge cases.
```

---

## 5.2 Text storage: chunked piece chain

Normal files and sparse-edit huge files use a piece model.

```rust
enum PieceSource {
    Original,
    AddMem(AddChunkId),
    AddSpill(SpillFileId),
}

struct Piece {
    source: PieceSource,
    start: u64,
    len: u64,
    flags: PieceFlags,
}

struct PieceChunk {
    pieces: [Piece; N],           // default N ≈ 256–512
    len: u16,
    byte_len_sum: u64,
    newline_count_sum: u32,
}

struct PieceChain {
    chunks: Vec<PieceChunk>,
    total_len: u64,
    piece_count: u64,
}
```

The chain is **not** a rope.

Reasons:

```text
1. The editor is byte-offset native.
2. The original file remains external and chunk-read.
3. Edits are sparse.
4. Metadata stays compact and cache-friendly.
5. Middle edits shift only inside bounded chunks.
```

### Piece lookup

Use a two-level index:

```text
chunk prefix byte sums
piece byte sums inside chunk
```

Lookup:

```text
byte offset -> chunk by prefix search -> piece by local scan/binary search
```

For default 256–512 pieces per chunk, local scans are bounded and cache-local. If piece count becomes pathological, metadata-only compaction coalesces adjacent compatible pieces during idle.

### Cost

```text
Bundle: small.
Memory: piece metadata only; original bytes stay outside RAM.
Performance: strong locality; better than pointer-heavy tree for the locked edit model.
```

---

## 5.3 Add-buffer storage

Inserted bytes go into a spillable add buffer.

```rust
struct AddStore {
    mem_chunks: Vec<AddMemChunk>,
    mem_cap: usize,
    spill: Option<TempSpillFile>,
}
```

Rules:

```text
small inserts: memory chunks
large paste/AI edits: OS temp spill
spill path: OS temp only
pieces reference add storage by source + byte range
```

This prevents large paste/model-generated insertions from becoming unbounded RSS.

### Cost

```text
Bundle: small.
Memory: hard cap for inserted bytes retained in RAM.
Performance: normal typing stays memory-fast; huge inserts pay temp-file I/O after threshold.
```

---

## 5.4 Newline index

Below huge-file threshold:

```text
full newline index
u64 byte offsets per line
```

Above huge-file threshold:

```text
no global newline index by default
visible rows only
byte/percent navigation
exact goto-line unavailable unless explicitly indexed
```

Normal-file index:

```rust
struct LineIndex {
    newline_offsets: Vec<u64>,
    complete: bool,
}
```

Open behavior is progressive:

```text
show first viewport immediately
build line index cooperatively after open
line/column UX becomes exact once mapping is ready
```

This reconciles fast open with normal-file exact navigation.

### Cost

```text
Bundle: small.
Memory: about 8 bytes per line plus vector overhead below threshold.
Performance: strong normal-file navigation; huge files avoid the cost entirely.
```

---

## 5.5 Invalid UTF-8

Core is byte-preserving.

Default rendering:

```text
invalid byte -> \xNN
```

Editing policy:

```text
normal text insertion must be valid UTF-8
existing invalid byte regions are preserved
edit commands cannot silently split/corrupt escaped byte tokens
forced escaped-text mode may replace explicit byte ranges
```

This is not a hex editor. Binary-like files open read-only by default; the user must explicitly force text-edit mode.

### Cost

```text
Bundle: small.
Memory: none beyond display decoding.
Performance: ASCII fast path dominates; invalid bytes are rare-path rendering.
```

---

# 6. Huge-file model

Default huge threshold:

```text
256 MiB, configurable
```

Above threshold:

```text
sparse editing allowed until memory pressure/config flips to view/search/follow read-only
no global line index
no replace-all
literal streaming search only
viewport lexical highlighting only
statusline indicators: HUGE, RO, NOIDX, BIN, FOLLOW
```

Huge-file operations:

```text
open: reconstruct first viewport from byte window
scroll: row-based where known, byte-window seek when unindexed
search: streaming chunks, cancellable
goto: byte offset / percent / visible-window-relative
syntax: viewport/nearby lexical only
LSP/completion: disabled unless plugin explicitly supports bounded visible region
```

The claim should be precise: this architecture can keep **input and viewport render work** sub-millisecond on large files. It cannot make disk, terminal, SSH, network filesystems, or plugin/model latency sub-millisecond.

---

# 7. Edit transaction model

All core/plugin edits use sorted, non-overlapping byte-range replacements.

```rust
struct Edit {
    start: u64,
    end: u64,
    replacement: ByteSliceRef,
}

struct EditTransaction {
    target_buffer: BufferId,
    target_revision: u64,
    edits: SmallVec<Edit>,
    grouping: UndoGroupKind,
}
```

Validation:

```text
edits sorted
edits non-overlapping
start <= end <= buffer_len
target revision matches current buffer revision
read-only/follow/binary restrictions checked
replacement byte ownership bounded
huge-file transaction cap enforced
```

Application algorithm:

```text
1. validate transaction
2. collect inverse pieces for undo
3. walk piece chain once
4. split boundary pieces
5. replace target ranges with add-store pieces
6. coalesce adjacent compatible pieces opportunistically
7. patch normal-file newline index synchronously by default
8. increment buffer revision
9. mark affected viewport rows dirty
```

No line/column transaction format exists in core. LSP/plugins must convert before submission.

### Cost

```text
Bundle: smallest edit model.
Memory: transaction + inverse pieces only.
Performance: byte-native, huge-file compatible, invalid-byte compatible.
```

---

# 8. Undo / redo

Core undo is session-only and memory-capped.

```rust
struct UndoEntry {
    forward: EditTransaction,
    inverse: PieceSnapshotTransaction,
    byte_cost: usize,
    group_id: UndoGroupId,
}
```

Grouping:

```text
default grouped typing undo
break on pause, cursor move, selection change, save, command, explicit boundary
configurable thresholds
```

Memory cap:

```text
fixed byte cap, configurable
drop oldest undo entries when exceeded
```

Durable git-like checkpoints/history are plugin-owned. Core exposes state snapshots and byte transactions but does not persist durable history.

### Cost

```text
Bundle: small.
Memory: hard-capped.
Performance: safe; undo stores piece refs where possible rather than copying original bytes.
```

---

# 9. Save/write strategy

## 9.1 Normal files

Below huge threshold:

```text
atomic temp-file write + rename
preserve basic Unix mode
mtime/size check before save
brief advisory lock during save
symlinks followed
no backup files in core
```

Advanced metadata such as ownership, ACLs, xattrs, timestamps, backups, and deployment metadata are plugin/hook-owned.

## 9.2 Huge sparse-edited files

Adaptive save:

```text
length-preserving edits: in-place pwrite where safe
append-only edits: append/truncate where safe
length-changing middle edits: streaming rewrite/export path preferred
in-place shifting: only explicit, warned, crash-risk path
```

Do not lie to the user: arbitrary length-changing middle edits in a multi-GB file cannot be made both instant and crash-safe without extra storage or a full rewrite.

### Cost

```text
Bundle: small.
Memory: streaming save uses bounded buffers.
Performance: strong for length-neutral sparse edits; length-changing edits pay unavoidable I/O.
Safety: normal files safe; huge in-place patch weaker and must be explicit/warned.
```

---

# 10. Search and replace

Core search:

```text
literal substring only
case-sensitive/insensitive configurable
streaming whole-file search
bounded result set
cancellable
current search only
```

No regex in core.

Core replace:

```text
replace next
replace all in current normal buffer
replace in selection/range
huge-file replace-all disabled
regex/project-wide replace plugin-owned
```

Streaming search reads chunks cooperatively and yields to input/render after fixed work slices.

### Cost

```text
Bundle: smallest.
Memory: bounded active result set only.
Performance: safe; no persistent index and no regex engine.
```

---

# 11. Rendering architecture

## 11.1 Frame model

Locked:

```text
single reusable output buffer
alternate screen
direct ANSI
native cursor by default
hybrid style reset: reset at row boundary, style deltas inside row
structural clears only by default
```

Frame buffer:

```rust
struct FrameBuf {
    bytes: Vec<u8>,       // preallocated
    cap_soft: usize,
}
```

Rules:

```text
clear(), do not free
no allocation per cell
no allocation per row
no allocation per style span
flush once when possible
chunked flush only if buffer exceeds configured cap
```

---

## 11.2 Dirty model

```rust
struct DirtyState {
    tabline: bool,
    statusline: bool,
    left_slot: bool,
    right_slot: bool,
    bottom_slot: bool,
    editor_rows: BitSet,      // viewport-height bits
    layout_structural: bool,
}
```

Granularity:

```text
fixed UI regions: region-dirty
editor content: row-dirty
no cell-level dirty tracking
```

Structural changes:

```text
resize
pane open/close
theme reload
tabline geometry change
```

Normal edits:

```text
dirty affected rows
dirty statusline
dirty cursor row if selection/cursor changed
```

Plugin decoration updates:

```text
apply only if buffer revision matches
dirty affected visible rows only
drop stale decoration versions
```

---

## 11.3 Render cache

Default:

```text
viewport render cache only
hybrid row representation
```

```rust
struct RenderRow {
    buffer_start: u64,
    buffer_end: u64,
    display_segments: SmallVec<DisplaySeg>,
    style_runs: SmallVec<StyleRun>,
    hash: u64,
    valid_for_revision: u64,
}
```

Cells are generated only for dirty rows.

Pipeline:

```text
buffer byte range
-> chunk/piece reader
-> UTF-8/escaped-byte decoder
-> horizontal viewport clip
-> tab expansion
-> width calculation
-> selection overlay
-> diagnostics/fold/decor overlays
-> syntax lexical spans
-> style-run merge
-> ANSI row emission
```

Unicode:

```text
ASCII fast path
basic Unicode width in core
richer grapheme/bidi metadata plugin-assisted only
```

---

## 11.4 Scrolling

Hybrid scrolling:

```text
normal/known line regions: row scrolling
huge unindexed regions: byte-window seeking + local row reconstruction
```

For small vertical scrolls inside the editor region:

```text
optional terminal scroll-region optimization:
  scroll existing editor rows
  paint only newly exposed rows
```

For large jumps:

```text
mark editor viewport rows dirty
reconstruct visible rows from byte window / line index
```

This is the right differential strategy: do not repaint whole screen, do not maintain a full cell grid, and do not keep a global render cache.

---

## 11.5 Rendering loop

```text
loop:
  poll fds/timers
  drain input first
  apply all pending input mutations
  process only cheap watcher/plugin/timer events
  run bounded cooperative work slice
  if dirty:
      build frame from dirty regions
      write frame buffer
      move native cursor
```

Input has priority over paint:

```text
if new input arrives before stale render completes:
    skip/coalesce old render
```

No fixed FPS loop. No async runtime. No thread pool.

### Cost

```text
Bundle: small.
Memory: viewport cache + one frame buffer + dirty bits.
Performance: strongest practical terminal path; terminal bandwidth may dominate over CPU.
```

---

# 12. Terminal backend

Use a tiny Unix terminal layer only for:

```text
raw mode / termios
signals
possibly polling fd primitives
```

Core owns:

```text
ANSI output
input parser
mouse protocol
bracketed paste
focus events
Kitty/CSI-u-style enhanced keyboard probing
tmux/SSH fallback behavior
screen diff
cursor control
```

Crossterm is rejected because it is explicitly cross-platform and broader than the locked Unix-native target. ([docs.rs][3])

### Cost

```text
Bundle: near-minimal.
Memory: negligible.
Performance: deterministic; no terminal framework hidden work.
```

---

# 13. File watching

Backend:

```text
Linux: inotify
macOS/BSD-style: kqueue / native vnode events
```

The Linux inotify API monitors filesystem events for files or directories. ([man7][11]) Apple’s kqueue documentation describes `EVFILT_VNODE` as monitoring events on a file descriptor. ([Apple Developer][12])

Scope:

```text
open files
visible/expanded explorer directories
```

No recursive whole-workspace watch by default.

Event policy:

```text
default debounce + stat confirmation
configurable immediate mode for follow/log workflows
clean buffer: auto-reload with statusline notice by default
dirty buffer: statusline notice; external cache plugin-owned
follow mode: hard reload immediately
```

The locked external dirty-cache policy is:

```text
core records change metadata
merge/session plugins decide whether to snapshot external versions
```

### Cost

```text
Bundle: small-medium OS-specific code.
Memory: low; watcher scope is narrow.
Performance: fast detection without polling; debounce avoids save-burst storms.
```

---

# 14. Plugin architecture

## 14.1 Process model

All plugins are out-of-process executables.

```text
transport: stdin/stdout compact binary framed protocol
production core: no JSON
dev adapter: optional external JSON-lines ↔ binary bridge
```

Frame shape:

```text
u32 length
u16 message_type
u16 flags
u32 request_id
u64 resource_id
u64 resource_revision
payload...
```

Rules:

```text
bounded frames
version handshake
strict validation
request IDs for commands/queries
per-resource versions for state streams
drop stale updates
```

## 14.2 Plugin authority

Plugins may:

```text
register commands
provide typed command args
provide structured widget data
provide lexical/decor spans
provide diagnostics/fold ranges
return byte-range edit transactions
write files directly if trusted workspace writer
run AI/model jobs
run external tools
```

Plugins may not:

```text
draw raw ANSI
receive raw key stream
own core buffer mutation
own undo integrity
block typing/rendering
replace the text engine
load in-process
```

## 14.3 Plugin views

Core widgets only:

```text
lists
trees
tables
text blocks
logs
prompts
buttons/actions
```

Plugin input is structured:

```text
item selected
button pressed
search text changed
prompt submitted
tree row expanded/collapsed
```

No raw input stream.

## 14.4 Plugin failure

```text
statusline notification
details in bottom/job pane
restart lifecycle API required
restart policy configurable per plugin
```

### Cost

```text
Bundle: low core; plugins separate.
Memory: plugin memory outside core.
Performance: safe if IPC is batched and stale frames are dropped.
```

---

# 15. AI/model integration

AI plugins support:

```text
command-only AI actions
bottom chat/jobs pane
streaming output
core-validated current-buffer edit transactions with user confirmation
trusted direct workspace writes
```

No inline ghost text by default.

Direct writes:

```text
plugin may write anywhere under explicit workspace root once trusted
core watcher sees changes as external file changes
follow mode hard-reloads
normal dirty buffers show conflict status
```

### Cost

```text
Bundle: no AI SDKs in core.
Memory: model/provider memory outside core.
Performance: no AI in input/render hot path.
```

---

# 16. LSP / syntax / diagnostics

LSP is plugin-owned.

The first-party LSP plugin baseline:

```text
diagnostics
completion
goto definition
references
```

Completion:

```text
debounced automatic below file-size threshold
disabled in huge-file mode
async/cancellable/stale-safe
```

Syntax highlighting:

```text
default Rust/Markdown highlighters: viewport lexical only, plugin executables
full syntax-aware parsers: optional plugins
core receives decorations only
```

Diagnostics state:

```text
plugin-owned
core renders active visible markers/lists only
no persistent core diagnostic history
```

### Cost

```text
Bundle: core stays small.
Memory: parsers/LSP/server state external.
Performance: stale plugin updates dropped; visible rows only dirtied.
```

---

# 17. File picker / explorer-lite

Core includes a palette-based lazy tree picker.

```text
root without workspace: process current directory
root with -w: explicit workspace root
directories as CLI positional args: rejected unless -w
```

Traversal:

```text
lazy recursive tree
only expanded branches loaded
typing triggers lazy expand-on-search
walk is bounded/cancellable/cooperative
```

Ignore rules:

```text
global config ignores
one root .gitignore
minimal parser subset only
full Git fidelity plugin-owned
```

Supported `.gitignore` subset:

```text
comments
simple names
directory suffix /
*
?
basic !
```

### Cost

```text
Bundle: small.
Memory: bounded by expanded tree/results.
Performance: no full recursive index; fuzzy search improves progressively.
```

---

# 18. Event loop and cooperative work

Locked:

```text
single-threaded poll loop
no async runtime
no thread pool
default cooperative fixed work slices
configurable time budgets
```

Long tasks must be incremental:

```text
directory walk
streaming search
file load
newline index build
piece compaction
save streaming
plugin IPC drain
watcher event handling
```

Rule:

```text
Every long task must expose step(max_items) or step(max_bytes).
Every step must be cancel-safe.
Input cancels metadata compaction immediately.
```

Pseudo-loop:

```rust
loop {
    let events = poll_once(next_timer);

    input_queue.drain_all();
    apply_input_first();

    watcher.drain_bounded();
    plugins.drain_bounded();
    jobs.drain_bounded();

    scheduler.run_core_tasks_fixed_slices();

    if dirty.any() && !input_pending() {
        renderer.paint(&mut frame_buf, &dirty);
        terminal.write_all(&frame_buf);
        dirty.clear_painted();
    }
}
```

### Cost

```text
Bundle: small.
Memory: small task state machines.
Performance: strongest latency predictability; throughput sacrificed when necessary.
```

---

# 19. Memory caps and degradation

Core enforces hard caps and adaptive degradation.

Core caps:

```text
original chunk cache
add-buffer memory before spill
undo memory
render cache
file picker loaded entries/results
search result set
plugin widget visible state
line index threshold
piece metadata threshold
frame buffer soft cap
```

Plugin budgets:

```text
soft budgets passed to plugins
plugins self-police
core does not kill by RSS by default
```

Degradation priority is configurable. Default:

```text
disable full/plugin highlighting
reduce diagnostics/decorations
shrink search/job result buffers
switch huge files to read-only view/search/follow
reject new heavy plugin views
```

### Cost

```text
Bundle: small.
Memory: bounded core.
Performance: avoids OS-level surprise failure.
```

---

# 20. Binary size strategy

Profiles:

```text
core: minimal dynamic Unix binary
standard/dev: build flags / packaging stance only
plugins remain separate executables
```

Linking:

```text
profile-dependent
core prefers smallest dynamic Unix binary
static/self-contained packages optional
```

Allocator:

```text
system allocator only
no jemalloc/mimalloc
hot paths use reuse, caches, and caps
```

Telemetry:

```text
none
```

Perf tracing:

```text
dev builds only
```

Crash path:

```text
restore terminal
leave alternate screen
write crash log
print path to stderr
```

### Cost

```text
Bundle: smallest practical product architecture.
Memory: no plugin/process memory unless launched.
Performance: no tracing/telemetry/runtime overhead in core.
```

---

# 21. Rejected features from core

These are explicitly out:

```text
mmap
rope buffer
regex
tree-sitter
LSP client
scripting runtime
WASM runtime
async runtime
thread pool
terminal UI framework
in-process plugins
raw plugin drawing
raw plugin input
multi-cursor
soft wrap
embedded PTY
rich Markdown/HTML preview
diff UI
recursive workspace watcher
session persistence
backup files
trash integration
full Git ignore engine
full grapheme/bidi engine
```

Most are plugin-owned or external-tool-owned.

---

# 22. Implementation order

## Stage 0 — core skeleton

```text
CLI: file list mode and -w workspace mode
raw terminal enter/restore
alternate screen
input parser
single frame buffer
tabline/statusline
empty buffer
basic cursor movement
```

## Stage 1 — byte storage

```text
pread chunk cache
piece chain
add store
byte-range transactions
render visible rows
invalid UTF-8 escapes
```

## Stage 2 — editing

```text
insert/delete
linear/block selection
undo/redo memory cap
save normal files
dirty close prompt
read-only mode
```

## Stage 3 — huge-file behavior

```text
256 MiB threshold
degraded status flags
byte-window scrolling
streaming literal search
replace-all disabled in huge mode
progressive open
```

## Stage 4 — renderer hardening

```text
dirty rows
fixed-region dirty flags
render cache
style span compiler
theme table
single-buffer ANSI output
scroll optimization
```

## Stage 5 — filesystem

```text
native watchers
follow mode
clean auto-reload
dirty external-change notice
mtime/size save warning
advisory lock-on-save
file operations
```

## Stage 6 — palette/file picker

```text
command palette typed args
lazy recursive tree picker
lazy expand-on-search
minimal root .gitignore parser
global ignore globs
```

## Stage 7 — plugin host

```text
binary framed stdio protocol
version handshake
request IDs
resource versions
stale update drop
structured widgets
plugin restart API
```

## Stage 8 — first-party optional plugins

```text
Rust lexical highlighter
Markdown lexical highlighter
session/recovery plugin
LSP plugin
AI command/chat plugin shell
external search plugin examples
```

---

# 23. Final architecture sentence

Build a **byte-addressed, pread-backed, chunked-piece-chain terminal editor** with a **single-threaded input-priority event loop**, **viewport-only rendering**, **single reusable ANSI frame buffer**, and **strict out-of-process plugins**. Reject ecosystem engines that force in-memory ropes, mmap, terminal frameworks, parser stacks, broad cross-platform abstractions, or plugin code inside the editor process.

[1]: https://docs.rs/qem?utm_source=chatgpt.com "qem - Rust"
[2]: https://docs.rs/ropey?utm_source=chatgpt.com "ropey - Rust"
[3]: https://docs.rs/crossterm/?utm_source=chatgpt.com "crossterm - Rust"
[4]: https://docs.rs/opentui_rust?utm_source=chatgpt.com "opentui_rust - Rust"
[5]: https://opentui.com/?utm_source=chatgpt.com "OpenTUI - Terminal UIs"
[6]: https://docs.rs/ratatui/latest/ratatui/widgets/index.html?utm_source=chatgpt.com "ratatui::widgets - Rust"
[7]: https://docs.rs/libloading/?utm_source=chatgpt.com "Crate libloading - Rust"
[8]: https://github.com/rust-lang/reference/blob/master/src/items/external-blocks.md?utm_source=chatgpt.com "reference/src/items/external-blocks.md at master"
[9]: https://docs.rs/abi_stable/?utm_source=chatgpt.com "abi_stable - Rust"
[10]: https://man7.org/linux/man-pages/man2/pread.2.html?utm_source=chatgpt.com "pread(2) - Linux manual page"
[11]: https://man7.org/linux/man-pages/man7/inotify.7.html?utm_source=chatgpt.com "inotify(7) - Linux manual page"
[12]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kqueue.2.html?utm_source=chatgpt.com "Mac OS X Manual Page For kqueue(2)"

