# Updating the pinned Reverie revision

Hermit depends on [Reverie](https://github.com/facebookexperimental/reverie) as
a git dependency, pinned to a **specific commit** (`rev = "<hash>"`) rather than
a moving `branch = "main"`. Pinning makes builds reproducible: when hermit's
tests pass, the exact Reverie commit is recorded in the manifests (and is not
silently changed by an upstream push).

The pin has two distinct purposes. For an old checkout it is an archival record:
`cargo build` can reproduce the Reverie revision that checkout used. For current
testing it is a pointer into Reverie's **linear main history**, judged by
**ancestry and monotonic advance** rather than by equality with a live tip:

1. **Ancestry** — the pin must be an ancestor of `rrnewton/reverie:main`.
   **Lagging is legitimate.** A pin behind the tip is a pin; a pin required to
   equal the tip is not.
2. **Monotonic** — relative to the landing-base pin it may only advance forward
   or remain unchanged. Ancestry alone would accept a pin walked *backwards*,
   because an ancient commit is also an ancestor. The floor is the pin recorded
   at the base the change would land on (`--base-ref`, default `origin/main`).
3. **A conflict resolves to the newer pin** — and this is enforced *by* rule 2
   rather than by a separate mechanism. Resolving a `Cargo.lock` conflict to the
   older side regresses the pin below the base, which rule 2 refuses. Conflict
   resolution is exactly where a silent regression would otherwise land.

An off-history, backward, or sideways pin cannot produce current validation
evidence.

> **Historical, and it was correct when written.** Until 2026-08-08 this
> document and the checker required the pin to **equal** the live
> `rrnewton/reverie:main` tip, and said in as many words that being an ancestor
> was not sufficient. That rule was introduced in `f21b22ed` (2026-08-05) and
> superseded three days later by `e35594ad`, *"Judge the Reverie pin by ancestry
> and monotonicity, not tip equality"* (owner-approved, 2026-08-08). It is
> recorded here rather than deleted because someone reading git history will
> meet it and deserves to know it was once the rule.
>
> Why it was replaced: equality made the comparand a **live moving ref**, so the
> verdict was a property of the tree *and the instant you looked*. Two runs over
> a byte-identical tree disagreed with nothing changed locally, and at roughly
> 16.6 Reverie commits/day the gate fired on nearly every tick.

These two clauses work together. Ancestry rejects abandoned, rewritten, and
unmerged Reverie commits. Monotonicity rejects an ancient-but-still-ancestral
commit and makes conflict resolution unambiguous: when two sides carry different
pins, always choose the newer side.

## Policy gate

`scripts/check-reverie-pin.rs` is the canonical verifier source. The tracked
`ci/run-reverie-pin-check.sh` launcher compiles it with `rustc`, derives its
scope from `git ls-files`, and checks every tracked `Cargo.toml` and
`Cargo.lock`. Every Reverie revision in that tracked Cargo dependency metadata
must be identical to the others, must be an **ancestor** of
`rrnewton/reverie:main`, and must not regress below the base's pin.
The checker reports the manifest, lockfile, pinned-file, revision-entry, and
main-history relationship on every run so a green result states its coverage.
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

There is no stale-pin, ancestry, or regression override in testing. Local
validate, both committed DAGs, hosted portable CI, the merge gate, and validate
receipt production all invoke the same fail-closed rule. Historical source
remains buildable at its recorded revision; it does not create current
validation evidence until its pin satisfies ancestry and monotonicity against
the base it would land on. **Reverie main may advance after the Hermit commit
without invalidating that evidence.**

Monotonicity needs a base, and an *unresolvable* base is not a pass. If the
floor cannot be resolved — no `origin/main`, a depth-1 clone, an incoherent base
pinning two revisions — the checker must not treat a skipped monotonicity check
as a passing one. A caller that genuinely has no base declares it with
`--no-base` rather than letting the check quietly disappear. Ancestry also needs
Reverie's **commit graph**: `ls-remote` returns only a tip and can answer no
reachability question, so the checker fetches a blobless bare graph (about 1s
and 1.3MB) and fails closed if the authority tip cannot be resolved at all.

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

Core build references also occur outside Cargo metadata, including the Buck
build's pin. hermit-cli/BUCK, which fbsource shares, takes it from this tree's
release values file, `shim/hermetic_infra/hermit/fb-only/release.bzl` (fbsource
keeps its own `fb-only/release.bzl`), so a pin bump edits that file and never
hermit-cli/BUCK. The updater carries full-length occurrences mechanically
and checks the selected Buck and LiteInst cache bindings before compiling.
Derive the current sites by search rather than a fixed list:

```bash
git grep -l "$(./ci/run-reverie-pin-check.sh --print-pin)" -- \
    ':!*Cargo.toml' ':!*Cargo.lock'
```

The former `ci/run-with-reverie-dbt-budget.sh` and its calibration chain in
`ci/configure-build-jobs.sh` were retired after DynamoRIO moved to the extension
repository. No core pin bump requires an SDK budget carry. The updater retains
an exclusion for that retired filename, so restoring it cannot mechanically
rewrite a measured claim. Any new `*budget*.sh` using `expected_pin=` needs a
production calibration decision path: private-static's unchanged `full_pins`
check requires that value to match the Reverie pin and otherwise refuses after
a bump. Removing the old wrapper does not resolve that future design choice.

## How to advance the pin

Advance every derived manifest and lockfile site to the current Reverie main tip
in one command:

```bash
with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest
```

This is a convenient forward-update operation, not the gate's acceptance
criterion: a pin that lags the tip but is ancestral and non-regressing already
passes. The checker derives every manifest from `git ls-files`, replaces the old
revision in those manifests, and asks Cargo to re-resolve both tracked lockfiles.
LiteInst staging derives its cache suffix from that same recorded revision, so
there is no cache-key list to update. Review the resulting diff, then run the
test suite; a Reverie change can alter interception behavior even when it
compiles.

The updater also carries derived core references and checks build viability.
Review those edits and verify the complete candidate before committing:

1. **Confirm the tree is consistent** before committing:

   ```bash
   ./ci/run-reverie-pin-check.sh          # expect rc=0 and an ancestry/monotonicity verdict
   ./scripts/validate.rs --self-test       # expect the Rust plan/pin-authority brackets to pass
   ```

2. **Paste the private-static pin-bump report into the pull request body.**
   The private-static producer admits the Reverie runtime sources whose rows it
   derives from the pin itself, so a bump changes what it accepts without any
   edit in `hermit-install/`. The report is where that change gets reviewed:

   ```bash
   python3 -B hermit-install/private-static/pin_bump_report.py <old-pin> <new-pin>
   ```

   It lists every derived Reverie row whose content changed, with both digests
   and a diff stat. A changed `trap.rs` also gets its gate-block digests, and
   a changed x86_64 block reads `CHANGED, review the gate block; differs from
   the reviewed guard, the build refuses`. No `R ` lines means no reviewed
   runtime source changed. See `hermit-install/private-static/README.md`.

Both remaining steps verify and report the pin update. Keep their results with
the pull request so the consistency checks and runtime-source changes can be
reviewed together.

## Notes

- `Cargo.lock` and `liteinst-runtime-build/Cargo.lock` are tracked. The runtime
  builder is an isolated workspace, so update and commit both lockfiles with
  every pin change.
- When a merge or rebase conflicts in any pin-bearing manifest or lockfile,
  resolve every site to the newer Reverie commit. The policy gate rejects the
  older side as a regression even when that older commit is still on main.
- To point at a fork instead of upstream (e.g. for the experimental
  `reverie-dbt` / `reverie-kvm` backends), change the `git =` URL as well as the
  `rev`, and keep all Reverie crates on the same source.
