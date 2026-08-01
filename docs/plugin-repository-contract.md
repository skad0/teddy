# Plugin repository contract

This is a maintainer agreement for repositories intended to appear in teddy's
local catalog. The exact parser and limits are normative in
[Plugin manifest reference](plugin-manifest.md); this document explains how to
publish and maintain a repository without duplicating that specification.

## Required identity and contents

* Use a stable plugin ID. Do not rename an ID to publish a new implementation;
  use a deliberate deprecation and replacement plan instead.
* Publish a root file named `manifest` with `format=teddy-plugin.v1`.
* Use a canonical public URL of the form
  `https://github.com/OWNER/REPOSITORY.git` and pin an exact 40-character
  lowercase hexadecimal commit in both the catalog entry and root manifest.
* The catalog descriptor and the manifest at that commit must be equal. A
  changed manifest requires a new immutable commit and catalog review.
* Provide an already-built executable under a relative
  `executable.<platform>` path for each supported platform. The executable
  speaks teddy's framed stdio protocol and replies to the v1 `HELLO` handshake
  with the same protocol version within two seconds.

A practical layout is:

```text
manifest
bin/                 # optional ordinary repository files
dist/<platform>/...  # prebuilt payloads referenced by manifest
```

The manifest path is fixed at the repository root; the executable paths are
relative paths in that same commit. The manager installs the selected prebuilt
file, not the checkout.

## Validation and trust boundary

Keep the repository within the enforced bounds: manifest/catalog files are at
most 64 KiB; IDs are at most 64 bytes and cannot use `teddy.*`; executable
paths are relative normal paths at most 4096 bytes; the Git tree is capped at
16,384 records; each distinct blob is at most 64 MiB and materialized blob
bytes are capped at 256 MiB; each Git command has a 30-second timeout and each
output stream an 8 MiB cap. The selected payload must be a regular executable,
not a symlink or an LFS pointer, and the tree must not contain symlinks or
submodules.

Do not rely on repository behavior outside the contract. Hooks, builds,
dependency installation, shell commands, SSH, local URLs, and unsafe absolute
or traversal paths are prohibited. The manager uses constrained Git and does
not execute a repository's build system.

The manager records and checks a SHA-256 receipt for the installed payload, but
there are no signature checks or sandboxing. Users must choose which catalog
and repositories they trust. The manager owns installation, receipts, journals,
immutable storage, and launcher orchestration; the plugin repository owns its
source, release artifacts, protocol implementation, and maintainer review.

## Maintainer workflow

1. Test the prebuilt executable on every advertised platform, including framed
   stdio startup and the v1 `HELLO` reply.
2. Commit the root manifest and artifacts; review the exact commit, paths, and
   executable modes.
3. Add or revise the local catalog entry only after reviewing that commit and
   confirming catalog/manifest equality. Pinning is explicit; the manager has
   no remote catalog or automatic update service.
4. For a release, publish a new immutable commit and catalog pin. Users choose
   `Update` explicitly; the manager retains the old installed payload.
5. For deprecation, stop advertising the catalog entry and document a
   replacement or end-of-life. Existing immutable installations are not
   automatically deleted or updated.
6. For a security or correctness incident, remove the affected pin from
   reviewed catalogs, notify users, and publish a corrected commit. Catalog
   removal alone cannot revoke an already installed local payload; users must
   disable/remove it through the manager.

## Pre-publication checklist

* [ ] Stable ID and canonical public GitHub HTTPS URL.
* [ ] Exact 40-hex commit is recorded identically in catalog and `manifest`.
* [ ] Root manifest parses as `teddy-plugin.v1` and uses safe relative paths.
* [ ] Prebuilt executable exists, is executable, and passes v1 `HELLO` startup.
* [ ] No symlinks, submodules, LFS pointers, hooks, builds, or dependency
      installation are required.
* [ ] Platform, size, tree, and Git output/timeout limits are satisfied.
* [ ] Catalog review, release notes, and incident/deprecation contacts are
      prepared.
