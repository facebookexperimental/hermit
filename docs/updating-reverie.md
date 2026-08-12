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

**That "zero occurrences outside the tracked Cargo metadata" clause stopped
being true after it was written.** The DBT build-budget calibration now pins the
revision in CI shell as well, and those sites are *not* Cargo metadata, so
neither the `git ls-files` set above nor `--update-to-latest` reaches them.
Derive them the same way — by search, not from a list in this document:

```bash
git grep -l "$(./ci/run-reverie-pin-check.sh --print-pin)" -- \
    ':!*Cargo.toml' ':!*Cargo.lock'
```

On 2026-08-08 at Reverie `fb963d90` that
returned three files holding 16 full-length occurrences:
`ci/run-with-reverie-dbt-budget.sh` (1), `ci/configure-build-jobs.sh` (2), and
`ci/test_harness.sh` (13). Treat the count as evidence the scope was exercised,
not as a fixed expectation. Note that `ci/configure-build-jobs.sh` also mentions
earlier revisions in short form inside its `CARRY TO` prose; the audit in
`ci/test_harness.sh` counts full-length occurrences only, so keep prose in short
form or the count breaks.

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

**That command is not the whole bump.** It covers Cargo metadata only, and it
prints `Reverie pin advanced to main tip <sha>` while the CI sites above still
hold the old revision — so an advance that stops here leaves the tree
inconsistent and `./ci/run-reverie-pin-check.sh` still BLOCKED. Three separate agents
rediscovered this on 2026-08-08. After running it, carry the CI sites too:

1. **Decide whether the build budget still applies.** This is the only step
   requiring judgement, and it must be made before rewriting anything.
   `ci/run-with-reverie-dbt-budget.sh` binds a *measured* clamp and threshold to
   one exact revision, deliberately, so a bump cannot silently reuse an earlier
   revision's calibration. The budget governs exactly one quantity: the elapsed
   time `reverie-dbt/build.rs` reports for a DynamoRIO content-key miss, hashed
   by `source_recipe_key()` over `{reverie-dbt/vendor/dynamorio,
   reverie-dbt/build.rs, $CMAKE, $CMAKE_GENERATOR}`. Diff those inputs between
   the old and new revision in a Reverie checkout:

   ```bash
   git -C <reverie> diff <old-pin>:reverie-dbt/build.rs <new-pin>:reverie-dbt/build.rs
   git -C <reverie> rev-parse <old-pin>:reverie-dbt/vendor/dynamorio \
                              <new-pin>:reverie-dbt/vendor/dynamorio
   ```

   Byte-identical inputs carry unchanged. **Changed bytes do not automatically
   mean recalibration** — the `108f9ab4 → fb963d90` carry changed `build.rs`
   while the calibration still held, because all eight changed lines were the
   `REVERIE_DBI_* → REVERIE_DBT_*` rename and nothing about what gets built
   changed. Judge whether the diff can affect *build time*, not whether it
   exists. If it can, recalibrate rather than carry, and record the measurement.
   Note the input paths themselves moved in that rename, so a query for
   `reverie-dbt/…` at an older revision returns nothing rather than a difference.

2. **Carry the revision across the CI sites**, then append a `CARRY TO <short>`
   block to `ci/configure-build-jobs.sh` stating the evidence for step 1. The
   existing chain of those blocks is the precedent and the format.

3. **Confirm the tree is consistent** before committing:

   ```bash
   ./ci/run-reverie-pin-check.sh          # expect rc=0 and an ancestry/monotonicity verdict
   ./ci/test_harness.sh audit-ci          # expect the budget-site counts to hold
   ```

Steps 2 and 3 are mechanical and should move into `--update-to-latest`; step 1
is a judgement and should stay a human decision that the tool refuses to skip.

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
