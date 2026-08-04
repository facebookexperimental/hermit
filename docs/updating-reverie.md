# Updating the pinned Reverie revision

Hermit depends on [Reverie](https://github.com/facebookexperimental/reverie) as
a git dependency, pinned to a **specific commit** (`rev = "<hash>"`) rather than
a moving `branch = "main"`. Pinning makes builds reproducible: when hermit's
tests pass, the exact Reverie commit is recorded in the manifests (and is not
silently changed by an upstream push).

Hermit's fork policy adds a consistency invariant: absent an explicitly
justified temporary exception, the pin must be an *ancestor* of the current
`rrnewton/reverie:main` tip — a real commit on main's history. The pin does not
have to equal the very latest tip; a pin that is merely behind main is fine.
This still catches the failure modes that matter — a typo, an orphaned SHA, or
an unmerged/side-branch commit that is not on main at all. (Bumping to the
latest tip is still encouraged so Hermit picks up merged correctness and
performance fixes — the demo5 investigation found exactly such a miss when
Hermit remained on `aa6f1283` and lacked the merged ptrace-notifier fast path —
but a behind-but-on-main pin no longer blocks CI.)

## Consistency lint

`scripts/check-reverie-pin.rs` derives its scope from `git ls-files` and checks
every tracked `Cargo.toml` and `Cargo.lock`. Every Reverie revision in that
tracked Cargo dependency metadata must be identical and must be an ancestor of
the live `rrnewton/reverie:main` tip (verified with a cheap treeless
commit-graph fetch). The checker reports the manifest, lockfile, pinned-file,
and revision-entry counts on every run so a green result states its coverage.
Tracked vendored Cargo metadata is included. Untracked/generated files and
nested submodule contents are excluded because Hermit does not track their
contents. Non-Cargo files are also outside this dependency-consistency check;
it does not certify arbitrary SHA links in source or documentation. The checker
fails closed when the remote cannot be checked. Run it locally through the
proxy:

```bash
with-proxy ./scripts/check-reverie-pin.rs
```

Install the tracked pre-commit gate once per clone/worktree repository:

```bash
scripts/setup-hooks.sh
```

Portable preland CI runs the same checker. A deliberate temporary stale pin must
state why latest main cannot be used. Local commits require a substantive reason:

```bash
HERMIT_STALE_REVERIE_PIN_REASON="Testing unmerged Reverie PR #123 before its merge" \
  git commit ...
```

The equivalent direct checker flag is `--allow-stale-reverie-pin "<reason>"`.
For preland CI, put this auditable line in the pull request body:

```text
Stale-Reverie-Pin-Reason: Testing unmerged Reverie PR #123 before its merge
```

An override only permits the exceptional commit; it does not make the pin
current. Remove it and repin to main as soon as the dependency lands.

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

1. Pick the target commit and confirm it exists upstream:

   ```bash
   with-proxy git ls-remote https://github.com/rrnewton/reverie.git refs/heads/main
   # or choose any specific commit hash you want to pin to
   ```

2. Replace the hash everywhere (one `sed` keeps them in sync):

   ```bash
   OLD=96693397ed60aa07c59ffeed4df3deed89b183e2
   NEW=<new-hash>
   grep -rl "$OLD" --include=Cargo.toml . | xargs sed -i "s/$OLD/$NEW/g"
   ```

3. Replace the old eight-digit short revision in all four LiteInst cache paths
   listed above with the new pin's first eight digits.

4. Re-resolve and build:

   ```bash
   with-proxy cargo update -p reverie   # refresh the lock for the new rev
   with-proxy cargo update --manifest-path liteinst-runtime-build/Cargo.toml
   with-proxy cargo build --workspace
   ```

5. Run the test suite before landing the bump; a Reverie change can alter
   interception/behavior even when it compiles.

## Notes

- `Cargo.lock` and `liteinst-runtime-build/Cargo.lock` are tracked. The runtime
  builder is an isolated workspace, so update and commit both lockfiles with
  every pin change.
- To point at a fork instead of upstream (e.g. for the experimental
  `reverie-dbi` / `reverie-kvm` backends), change the `git =` URL as well as the
  `rev`, and keep all Reverie crates on the same source.
