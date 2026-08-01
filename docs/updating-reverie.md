# Updating the pinned Reverie revision

Hermit depends on [Reverie](https://github.com/facebookexperimental/reverie) as
a git dependency, pinned to a **specific commit** (`rev = "<hash>"`) rather than
a moving `branch = "main"`. Pinning makes builds reproducible: when hermit's
tests pass, the exact Reverie commit is recorded in the manifests (and is not
silently changed by an upstream push).

Hermit's fork policy adds a freshness invariant: absent an explicitly justified
temporary exception, the pin must equal the current `rrnewton/reverie:main` tip.
A stale pin can silently omit already-merged correctness or performance fixes.
The demo5 investigation found exactly this failure mode when Hermit remained on
`aa6f1283` and missed the merged ptrace-notifier fast path.

## Freshness lint

`scripts/check-reverie-pin.rs` checks that every Reverie `rev` in Hermit's
manifests is identical and equals the live `rrnewton/reverie:main` tip. It fails
closed when the remote cannot be checked. Run it locally through the proxy:

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

The same `rev` appears in every crate that depends on a Reverie crate. Keep them
identical — mixing revisions can pull two incompatible `reverie` cores into one
build. As of this writing the deps are:

- `hermit-cli/Cargo.toml`
- `detcore/Cargo.toml`
- `detcore-dbi/Cargo.toml`
- `detcore-liteinst/Cargo.toml`
- `detcore-model/Cargo.toml` — `reverie-syscalls`
- `detcore-sabre/Cargo.toml`
- `detcore/tests/testutils/Cargo.toml`
- `hermit-install/Cargo.toml`
- `liteinst-runtime-build/runtime/Cargo.toml` — isolated constructor-runtime build

The first nine manifest locations above must stay on one exact revision. The
first eight hexadecimal digits also key LiteInst build caches. Update the
embedded short revision in all four locations so a new Reverie pin cannot reuse
or mislabel artifacts from the previous revision:

- `ci/dag/portable.json`
- `validate.sh`
- `hermit-install/build.rs`
- `hermit-cli/tests/common/liteinst.rs`

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
