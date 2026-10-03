# Plugin contract

Plugins are independent executables. The core is an out-of-process runtime
launcher and registry, not a package manager. The bundled `teddy-manager` is a
separately launched binary, enabled only when `TEDDY_PLUGIN_MANAGER` is an
existing absolute executable. The core does no Git or package work and never
accepts shell, argv, Git, or package authority.

The `teddytor` package ships `teddy-ai` as an implicit `src/bin/teddy-ai.rs`
binary. It is a bundled sibling plugin, not a separate crate or catalog
manager package. It registers `ai` and currently responds to invocation with
`ai shell: no provider configured`; it has no provider, API key, environment
setting, CLI arguments, model/configuration, or functional AI behavior. It can
be supplied directly through `TEDDY_PLUGINS`, but is not a manager-installed
catalog plugin.

The canonical implementation is [`src/plugin.rs`](../src/plugin.rs).

## v1 framing and handshake

Frames are little-endian with a 28-byte header: `payload_len: u32`,
`msg_type: u16`, `flags: u16`, `request_id: u32`, `resource_id: u64`, and
`resource_revision: u64`, followed by the payload. `PROTO_VERSION` is `1` and
`MAX_PAYLOAD` is 1 MiB. The existing message types are 1–10; launcher types
are reserved v1 types:

| Type | Constant | Direction |
|---:|---|---|
| 11 | `LAUNCHER_REQUEST` | manager → core |
| 12 | `LAUNCHER_RESPONSE` | core → manager |
| 13 | `LAUNCHER_EVENT` | core → manager |
| 14 | `WIDGET_INPUT` | core → plugin |

The core sends `HELLO` with the four-byte little-endian protocol version.
Plugins must reply with the same valid `HELLO` within two seconds. Contributions
are accepted only after this handshake. The response flag `0x1` requests
viewport frames.

Observed lifecycle states are `Starting`, `Running`, `Stopping`, `Backoff`,
and `Failed`. Unexpected failures use a bounded geometric retry schedule based
on persisted `backoff_ms` (`base`, `4×base`, `16×base`), with at most
`max_restarts` retries; `max_restarts: 0` disables retries. Invalid frames and
unauthorized launcher requests stop the offending slot.

## Widgets and structured input (spec §14.3)

`WIDGET` (4) carries data only; the core owns layout, drawing, and keys. The
payload is `kind: u8`, then `cols: u8` for tables only, then `count: u16`,
then per item (`depth: u8`, `flags: u8` for trees only) and a `u16`-length
UTF-8 string. Every string is control-sanitized. Each of the following is a
protocol violation that stops the slot: an unknown kind, a wrong shape,
trailing bytes, an unknown `WIDGET` frame flag, an unknown tree flag bit, or
a tree row flagged expanded without children. Core-side prompt and search text
is capped at 4 KiB.

| Kind | Name | Items | Keys |
|---:|---|---|---|
| 1 | list | rows | ↑/↓, Enter selects |
| 2 | tree | rows; flags `0x1` has children, `0x2` expanded; depth ≤ 32 | ↑/↓, Enter selects, →/← expand/collapse, typing searches |
| 3 | table | `cols` header cells, then whole rows (`count % cols == 0`) | ↑/↓ over body rows, Enter selects, typing searches |
| 4 | text | lines | ↑/↓ scroll |
| 5 | log | lines; the tail is shown | — |
| 6 | prompt | exactly one label | typing edits, Enter submits |
| 7 | actions | button labels on one row | ←/→ (or ↑/↓), Enter presses |

Selection keeps its v1 form: `WIDGET_EVENT` (5) with a `u32` row index (a body
row for tables). The other inputs use `WIDGET_INPUT` (14, core → plugin), so a
v1 plugin can never mistake them for a selection. Its payload is a tag byte:
`1` button pressed + `u32` index, `2` search text changed + UTF-8, `3` prompt
submitted + UTF-8, `4` tree row expand + `u32` index, `5` collapse + `u32`
index. Kind-1 lists never receive `WIDGET_INPUT`. Both event types echo the
widget's `resource_id` and `resource_revision`. Drop events with a stale
revision. The plugin owns tree expansion and filtering, and re-sends the
widget with a higher revision.

A `WIDGET` frame with flag `0x1` is the **Ctrl+T explorer/action surface**.
It never auto-opens. Ctrl+T focuses the lowest-id flagged widget of the first
running plugin that has one, in the fixed bottom slot. Unflagged widgets keep
the v1 behavior: the newest one opens over the editor area.

Each unexpected stop records its reason: the protocol violation, a missing
HELLO, a closed or failed pipe, or the exit status. It records the last
post-HELLO stderr lines with it. The core moves both into the bottom **jobs**
pane and opens that pane (spec §14.4). The statusline keeps a one-line notice.

## Dev JSON-lines bridge

`teddy-json-bridge` is a dev-only adapter. The production core never speaks
JSON. Register the bridge as the plugin executable, and set
`TEDDY_JSON_PLUGIN` to an absolute JSON-lines plugin executable. The bridge
runs that executable with no arguments. Each frame is one JSON object per
line:
`{"type", "flags", "request_id", "resource_id", "resource_revision"}` (missing
numbers are `0`) plus one payload field:

* `"text"`: UTF-8.
* `"hex"`: raw bytes.
* `"widget"`: plugin to core only, as
  `{"kind", "cols", "items": [...], "tree": [[depth, flags], ...]}`. It is
  validated with the core's own parser.

Core frames arrive as `"text"` when the payload is printable UTF-8, otherwise
as `"hex"`. A malformed line goes to stderr and ends the bridge, so it shows up
in the jobs pane.

## Launcher lane

Only the active stable slot `teddy.manager` may send `LAUNCHER_REQUEST`; it
cannot control itself. Other senders are stopped. Operations are exactly
`List`, `Enable`, `Disable`, `Reload`, and `Forget`.

The manager's catalog and installed inventory are local manager data, not core
registry data. Manager lifecycle actions use explicit cancel-first confirmation.
They mutate desired registry state with `Enable` or `Disable`, then observe
the result using bounded `List` polling; desired state and observed process
state must not be conflated. Launcher events are advisory until the correlated
operation response is received. Safe remove is ordered: `Disable`, bounded
`List` polling until absence or disabled `Failed` (fully reaped), `Forget`, then
journaled removal of the exact receipt/digest/path-matched immutable version.
The manager cannot send these operations for `teddy.manager`.

`Update` is a manager orchestration, not an additional launcher operation. It is
explicit and confirmation-gated, and is offered only for an enabled exact
manager-installed payload whose receipt, SHA-256 digest, and canonical launcher
path match. The candidate must be a different exact catalog commit. The manager
installs it side-by-side with the constrained Git path, durably stages the
transaction, disables/reaps/forgets the old record, then enables and bounded-
polls the candidate. Candidate failure or timeout starts exact old-payload
rollback; both immutable payloads remain. Disabled, external, ambiguous,
mismatched, self, and same-commit targets are refused.

Pending update journals are reconciled at startup/refresh only from correlated
launcher `List` state. A failed stage write or compensation retains the journal
and reports safe failure. This is not automatic updating: the manager does not
build, run hooks or dependencies, delete old payloads, sandbox, verify
signatures, or update itself.

Manager Git operations are fixed and bounded: each command has a 30-second
timeout, stdout and stderr are each capped at 8 MiB, tree parsing accepts at
most 16,384 records, each distinct blob is at most 64 MiB, and materialized
blob bytes summed across tree records are at most 256 MiB. These limits are
validation limits, not claims about network speed or sandboxing.

The following is the exact payload codec in `src/plugin.rs`:

* Request tag `0` is `List`, followed by `page: u16` (little-endian). Page
  numbering starts at zero; an out-of-range page is rejected.
* Request tag `1` is `Enable`: a length-prefixed ID (`u8` length plus UTF-8
  bytes), then a descriptor marker. Marker `0` is ID-only enable for an
  existing record. Marker `1` is a descriptor containing a `u16`-length UTF-8
  path, `u32 max_restarts`, `u32 backoff_ms`, and a confirmation byte (`0` or
  `1`). It is required for a new record. While registry recovery is pending,
  only a descriptor with confirmation `1` may mutate the registry; ordinary
  mutations are rejected.
* Request tags `2`, `3`, and `4` are `Disable`, `Reload`, and `Forget`, each
  followed by the length-prefixed ID.

IDs are at most 64 bytes, begin with a lowercase ASCII letter or digit, and
then contain only lowercase ASCII letters, digits, `.`, `_`, or `-`. The
`teddy.*` namespace is reserved for core slots. Descriptor paths are bounded
absolute executable paths.

Response tag `0` is `List`: `page: u16`, `next_page: u16` (`65535` means no
next page), then a `u16` count followed by records. The host emits bounded
pages of 32 records. Each record is
the length-prefixed ID, a `u16`-length UTF-8 path, an enabled byte, an observed
lifecycle-state byte, and `u32 max_restarts` plus `u32 backoff_ms`. Host state
codes are `0 Starting`, `1 Running`, `2 Stopping`, `3 Backoff`, and `4
Failed`; a registry record without an active runtime slot is reported as
`Failed`. Response tags `1`–`4` report the successful operation and ID. Tag
`255` carries a bounded `u16`-length UTF-8 error. Events use tags `1`–`4` for
successful state changes and carry an ID.

All launcher payloads are bounded and reject malformed, unknown, non-UTF-8,
or trailing data. Enable persists desired registry state before launching;
Forget requires a disabled, fully reaped record. A confirmed recovery save is
one-shot: after it commits, ordinary mutations use normal generation conflict
checking again. A save that commits but reports a directory durability warning
still keeps the runtime mutation, success response, and success event; the
warning is surfaced separately and is not retried. Desired registry state and
observed process state are distinct.

An explicit manager `Reload`, and recreation after `Forget`, reset retry
accounting and apply the current persisted executable and restart policy.

## Runtime safety

Registry and legacy slots are ordered and stable. On every transition away
from `Running`, the core closes that slot's widget mode and clears only its
owned contributions, viewport interest, and spans for that process generation.
The core resets viewport delivery when a slot becomes `Running` again.

Plugin stdin, stdout, and stderr are nonblocking. Stderr is polled and drained
after input under an aggregate bounded budget; lossy UTF-8 conversion replaces
control characters, including C0, C1, ESC, and DEL. Pre-HELLO stderr is not
shown as a notice. Oversized or malformed normal frames are rejected.

The existing messages remain unchanged: `REGISTER_COMMAND`, `COMMAND_INVOKE`,
`VIEWPORT`, `SPANS`, `EDIT_TX`, `EDIT_RESULT`, and `STATUS` retain their v1
meanings and payloads documented by the source implementation. `WIDGET` keeps
the kind-1 list payload and adds the kinds above.
The `VIEWPORT` name field is the buffer's display name: the file's basename
with control, C1, invalid UTF-8, and backslash bytes escaped as literal
`\xNN` (the same text shown in the tab), not the raw path bytes. Since `\`
itself is escaped as `\x5C`, replacing every `\xNN` with byte `0xNN`
recovers the basename exactly.
