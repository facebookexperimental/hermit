---
name: repo-cleanliness
description: "Keep hermit/ and reverie/ clean, focused implementation repos — no experiments, no ai_docs slop, no binaries, no vendored/nested git repos. Use before every commit and whenever staging files or deciding where an artifact belongs."
---

# Repo Cleanliness

`hermit/` and `reverie/` are **clean, focused implementation repositories**.
They contain product source, product tests, build configuration, and minimal
curated documentation — nothing else. Coordination state, experiments, research
notes, and heavy artifacts live in the `dev-hermit` **parent** workspace, not
here.

This skill is the standing rule for what may enter these repos and the
pre-commit check that enforces it. It applies to both `hermit/` and `reverie/`
(each carries its own copy, since they are separate Git histories).

## Why this matters

A product repo that accumulates experiment dumps, stray logs, vendored clones,
and binary blobs becomes slow to clone, hard to review, and impossible for a new
contributor to read. Meta exports these repos upstream; churn and slop leak into
that export. Keep the signal-to-noise ratio high: every tracked file should be
something a maintainer would knowingly keep.

## What belongs in hermit/ and reverie/

- Product source and public APIs.
- Product tests and regression coverage (at the narrowest useful layer).
- Build and lint configuration (`Cargo.toml`, `Cargo.lock`, CI workflows, etc.).
- **Curated, minimal** reference documentation: `AGENTS.md`, `README.md`,
  `CONTRIBUTING.md`, `docs/` that a new contributor actually needs.
- Agent skills under `.claude/skills/` (surfaced via the `.llms/skills` and
  `.agents/skills` directory symlinks — see "DRY skill layout" below).

## What does NOT belong here

- **Experiments and their outputs.** Reproducible experiments live in the parent
  at `~/work/dev-hermit/experiments/`. Do not create `hermit/experiments/` or
  `reverie/experiments/`. If you need to measure something, run it from a slot
  and record the durable result in the parent `experiments/` tree.
- **`ai_docs` slop.** Bulk AI research notes, design dumps, transient handoffs,
  and status logs belong in the parent `~/work/dev-hermit/ai_docs/`. Any
  `ai_docs/` kept inside a product repo must be **minimal, curated, durable
  reference** — not a scratch pad.
- **Binary files.** No compiled executables, object files (`.o`, `.a`, `.so`),
  archives (`.tar*`, `.tgz`, `.gz`, `.zip`, `.zst`), images, PDFs, database
  dumps, core dumps, profiler captures, VM images (`.img`, `.qcow2`, `.raw`,
  `.iso`), kernels (`bzImage`, `vmlinux`), or `initramfs*.cpio*`. Git LFS is not
  a workaround.
- **Nested / vendored Git repositories.** Never embed another project's clone
  (a nested `.git`) inside these repos. To depend on external code, record its
  **source URL and exact commit SHA** for reproducibility; do not vendor the
  checkout. (This is exactly how a 433M `experiments/gvisor/` clone with a 361M
  nested `.git` almost got swallowed into the parent — record hashes, never
  embed clones.)
- **Large or transient logs.** No multi-megabyte run logs, `*.perf.data`,
  coverage output, or `__pycache__/`. Keep evidence as summarized text
  (README/CSV/JSON) or in an ignored scratch dir.
- **Generated build artifacts.** `target/` and other build trees stay ignored.

## Where things actually go

| Artifact                         | Correct home                                   |
|----------------------------------|------------------------------------------------|
| Experiment + results             | `~/work/dev-hermit/experiments/<name>_YYYYMMDD/`|
| AI research / design / handoff   | `~/work/dev-hermit/ai_docs/`                    |
| Disposable notes, logs, probes   | `~/work/dev-hermit/scratch/` or an `ignored/` dir |
| Heavy/binary evidence            | ignored scratch or external store + a text manifest |
| Product source / test / doc      | the owning submodule (`hermit/` or `reverie/`) |

An experiment is durable only when another engineer can repeat it: record the
question, method, exact command, repo SHAs, host facts, seed, and text/CSV/JSON
results. Reference external code by URL + commit SHA — never by vendoring it.

## DRY skill layout

Skills are written **once** as real files and shared via directory symlinks, so
there is a single source of truth:

- `.claude/skills/` holds the **real** skill files (a flat `<name>.md`, or a
  `<name>/SKILL.md` directory for larger skills).
- `.llms/skills` and `.agents/skills` are **directory symlinks** to
  `../.claude/skills`. Do not duplicate a skill file into them.
- `CLAUDE.md` is a symlink to `AGENTS.md` for the same reason.

To add a skill, create one file under `.claude/skills/` — it appears
automatically under `.llms/` and `.agents/`. Never copy-paste a second copy.

## Pre-commit protocol

Before every commit, audit exactly what you are about to stage. Misplaced files
are cheap to fix before a commit and expensive to remove after.

```bash
git status --short                       # what's staged / untracked
git diff --cached --name-only            # exact staged paths
git diff --cached --numstat              # line counts — flag anything huge/binary
```

Verify, and **fix before committing** if any check fails:

1. **Right repo, right path.** Every staged file belongs in *this* repo and at a
   sensible path. Product code/tests/docs only. Nothing under `experiments/` or
   a bulk `ai_docs/`.
2. **No experiments.** No `hermit/experiments/` or `reverie/experiments/`
   additions — move them to `~/work/dev-hermit/experiments/`.
3. **No ai_docs slop.** Any `ai_docs/` change is minimal, curated, durable
   reference — not a scratch dump.
4. **No binaries.** Scan `git diff --cached --numstat`; a `-` in the line-count
   columns means a binary file. Inspect suspicious paths with `file` and `du`.
   Do not stage binaries or artifacts >2 MiB of text without coordinator
   approval.
5. **No nested git repos / vendored clones.** `git diff --cached --name-only |
   grep -E '/\.git(/|$)'` must be empty. Record external deps as URL + SHA.
6. **Only task-owned paths.** Do not sweep in another agent's in-flight changes.
   If the working tree is dirty with work you did not create, stage only your
   own paths explicitly — never `git add -A` past your ownership.

If a misplaced file is already staged: `git restore --staged <path>` (and move
it to its correct home) before committing. Never "commit now, clean up later."

## Quick audit

```bash
# From a product repo root — all of these should print nothing:
git ls-files | grep -E '^(experiments/|.*/experiments/)'        # stray experiments
git ls-files | grep -E '\.(o|a|so|tar|tgz|gz|zip|zst|img|qcow2|raw|iso|bin)$'  # binaries
git ls-files | grep -E '(^|/)\.git(/|$)'                        # nested repos
find . -name .git -not -path './.git' -prune -print            # embedded checkouts
```

A finding here is a cleanliness defect: relocate the file to the parent
workspace or an ignored dir, add a `.gitignore` guard, and keep the product repo
focused.
