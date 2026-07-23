# Updating the pinned Reverie revision

Hermit depends on [Reverie](https://github.com/facebookexperimental/reverie) as
a git dependency, pinned to a **specific commit** (`rev = "<hash>"`) rather than
a moving `branch = "main"`. Pinning makes builds reproducible: when hermit's
tests pass, the exact Reverie commit is recorded in the manifests (and is not
silently changed by an upstream push).

## Where the pin lives

The same `rev` appears in every crate that depends on a Reverie crate. Keep them
identical — mixing revisions pulls two incompatible `reverie` cores into one
build. As of this writing the deps are:

- `hermit-cli/Cargo.toml` — `reverie`, `reverie-ptrace`
- `detcore/Cargo.toml` — `reverie`, `reverie-ptrace`
- `detcore-model/Cargo.toml` — `reverie-syscalls`
- `detcore/tests/testutils/Cargo.toml` — `reverie`, `reverie-ptrace`

## How to bump

1. Pick the target commit and confirm it exists upstream:

   ```bash
   with-proxy git ls-remote https://github.com/facebookexperimental/reverie.git refs/heads/main
   # or choose any specific commit hash you want to pin to
   ```

2. Replace the hash everywhere (one `sed` keeps them in sync):

   ```bash
   OLD=96693397ed60aa07c59ffeed4df3deed89b183e2
   NEW=<new-hash>
   grep -rl "$OLD" --include=Cargo.toml . | xargs sed -i "s/$OLD/$NEW/g"
   ```

3. Re-resolve and build:

   ```bash
   with-proxy cargo update -p reverie   # refresh the lock for the new rev
   with-proxy cargo build --workspace
   ```

4. Run the test suite before landing the bump; a Reverie change can alter
   interception/behavior even when it compiles.

## Notes

- `Cargo.lock` is not tracked in this repo, so the `rev` in the manifests is the
  authoritative pin.
- To point at a fork instead of upstream (e.g. for the experimental
  `reverie-dbi` / `reverie-kvm` backends), change the `git =` URL as well as the
  `rev`, and keep all Reverie crates on the same source.
