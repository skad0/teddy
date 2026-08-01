# Plugin manifest reference

The manager accepts only the strict `teddy-plugin.v1` manifest. It is a
bounded UTF-8 text file of non-empty `key=value` lines (blank lines are
ignored). Keys and values are trimmed. Duplicate or unknown fields, malformed
lines, invalid UTF-8, and files larger than 64 KiB are rejected.

```text
format=teddy-plugin.v1
id=example
repo=https://github.com/example/plugin.git
commit=0123456789abcdef0123456789abcdef01234567
executable.darwin=bin/example
executable.linux=bin/example
```

The actual descriptor fields are:

* `format` — exactly `teddy-plugin.v1`.
* `id` — 1–64 bytes; starts with a lowercase ASCII letter or digit and then
  contains only lowercase ASCII letters, digits, `.`, `_`, and `-`. IDs in
  the `teddy.` namespace are rejected.
* `repo` — exactly a canonical public GitHub HTTPS URL of the form
  `https://github.com/OWNER/REPOSITORY.git`. Owners and repositories are
  non-empty, at most 100 bytes, and use only ASCII letters, digits, `-`, `_`,
  and `.`.
* `commit` — exactly 40 lowercase hexadecimal characters.
* `executable.<platform>` — one or more entries. The platform suffix is the
  lookup key (the bundled manager selects the current operating system). Each
  value is a relative repository path, at most 4096 bytes, with only normal
  path components; absolute paths, `..`, `.`, and backslashes are rejected.

There is no build command, hook, dependency declaration, submodule, LFS, or
shell field. The selected file must already be an executable regular file,
must not be a symlink, must be at most 64 MiB, and must not be a Git LFS pointer.
The repository manifest is read from the literal path `manifest` and must
match the catalog descriptor exactly. Git tree validation is bounded to 16,384
records; each distinct blob is at most 64 MiB, and the aggregate bytes
materialized by all blob records are at most 256 MiB. Each Git command has a
30-second timeout and each output stream is capped at 8 MiB.

Catalog files use `teddy-catalog.v1` blocks with the same descriptor fields,
separated by blank lines. A catalog is at most 64 KiB and 1024 records, and
must contain at least one record. It is local only; the manager does not fetch
catalogs or perform automatic updates.

## Installation receipt

The manager writes a `teddy-receipt.v1` receipt beside the payload with the
fields `format`, `id`, `commit`, `executable`, and a 64-hex-character SHA-256
`digest`. Reuse and removal require exact receipt identity, executable path,
payload path, and digest matches; missing, mismatched, or ambiguous versions
are refused. Installation promotes the staged directory by an atomic same-root
rename and does not replace an existing immutable version. Update transactions
record old and candidate receipt/payload identities in the manager state
directory; they retain both immutable versions while switching launcher state.
The pending update journal is removed only after completion or a safely
recorded compensation outcome.
