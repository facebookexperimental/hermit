# Updating the pinned Reverie revision

Hermit depends on [Reverie](https://github.com/facebookexperimental/reverie) as
a git dependency, pinned to a **specific commit** (`rev = "<hash>"`) rather than
a moving `branch = "main"`. Pinning makes builds reproducible: when hermit's
tests pass, the exact Reverie commit is recorded in the manifests (and is not
silently changed by an upstream push).

The pin has two distinct purposes. For an old checkout it is an archival record:
`cargo build` can reproduce the Reverie revision that checkout used. For current
testing it is a pointer: it must equal the live `rrnewton/reverie:main` tip.
Being an ancestor of main is not sufficient. A Hermit validation or pre-land
test against an older Reverie is blocked because it can miss already-landed
correctness fixes and produce evidence for a dependency version we no longer
ship.

## Currency gate

`scripts/check-reverie-pin.rs` is the canonical verifier source. The tracked
`ci/run-reverie-pin-check.sh` launcher compiles it with `rustc`, derives its
scope from `git ls-files`, and checks every tracked `Cargo.toml` and
`Cargo.lock`. Every Reverie revision in that
tracked Cargo dependency metadata must be identical and must equal the live
`rrnewton/reverie:main` tip. The checker reports the manifest, lockfile,
pinned-file, and revision-entry counts on every run so a green result states its
coverage.
Tracked vendored Cargo metadata is included. Untracked/generated files and
nested submodule contents are excluded because Hermit does not track their
contents. Non-Cargo files are also outside this dependency-consistency check;
it does not certify arbitrary SHA links in source or documentation. The checker
fails closed when the remote cannot be checked. Run it locally through the
proxy and the same launcher used by CI:

```bash
with-proxy ./ci/run-reverie-pin-check.sh
```

Install the tracked pre-commit gate once per clone/worktree repository:

```bash
scripts/setup-hooks.sh
```

There is no stale-pin override in testing. Local validate, both committed DAGs,
hosted portable CI, the merge gate, and validate receipt production all invoke
the same fail-closed rule. Historical source remains buildable at its recorded
revision; it does not create current validation evidence until rebased and
updated to latest Reverie main.

## Where the pin lives

The same revision appears in every tracked manifest and lockfile that resolves a
Reverie crate. Keep them identical — mixing revisions can pull two incompatible
`reverie` cores into one build. Do not maintain a path list in this document;
derive the current set exactly as the checker does:

```bash
git ls-files 'Cargo.toml' 'Cargo.lock' '**/Cargo.toml' '**/Cargo.lock'
```

Historical scope baseline: on 2026-08-04 at Hermit
`e8a0d8d3be3b53985dc898bb8e5cbb696a6a719f`, the derived set was 20 manifests
plus 4 lockfiles; 11 of those files held 47 Reverie revision entries. A search
for that revision, both full and eight-character forms, found zero occurrences
outside the tracked Cargo metadata. This dated baseline is evidence that the
scope was exercised when introduced, not a fixed expected count; the runtime
counts are authoritative as the repository changes.

## How to bump

Update every derived manifest and lockfile site in one command:

```bash
with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest
```

The checker derives every manifest from `git ls-files`, replaces the old
revision in those manifests, and asks Cargo to re-resolve both tracked lockfiles.
LiteInst staging derives its cache suffix from that same recorded revision, so
there is no cache-key list to update. Review the resulting diff, then run the
test suite; a Reverie change can alter interception behavior even when it
compiles.

## Notes

- `Cargo.lock` and `liteinst-runtime-build/Cargo.lock` are tracked. The runtime
  builder is an isolated workspace, so update and commit both lockfiles with
  every pin change.
- To point at a fork instead of upstream (e.g. for the experimental
  `reverie-dbt` / `reverie-kvm` backends), change the `git =` URL as well as the
  `rev`, and keep all Reverie crates on the same source.
