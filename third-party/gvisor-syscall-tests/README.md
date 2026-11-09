<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# gVisor syscall tests (vendored)

Vendored copies of [gVisor](https://github.com/google/gvisor)'s Linux
syscall tests (`test/syscalls/linux`, `test/util`), built as Hermit test
guests with Hermit's own Buck graph. The vendoring design is
`ai_docs/2026-11-04-gvisor-vendoring-design.md` in the dev-hermit
repository.

- Upstream commit: see `UPSTREAM_COMMIT` in this directory.
- License: Apache-2.0 (`LICENSE` in this directory, verbatim from
  upstream; every vendored file keeps its own Apache-2.0 header).

## Layout

- `upstream/` — upstream files as extracted from the pinned commit's
  `git archive` at refresh time. Never edit anything here.
  `build/upstream.sha256` records every file's hash from that refresh;
  `scripts/refresh-gvisor-tests.py --check` (run by the
  `check.gvisor_vendoring` DAG node) fails if any file is missing,
  extra, or edited since, or if the generated build files drift.
- `build/` — everything of ours: the generator (`gen_build.py`,
  `starlark_eval.py`), the generated neutral target list (`targets.json`)
  and combined-binary tables, the generated `BUCK`, and `support/`
  (combined-binary dispatcher, runfiles shim, environment gate).
  Generated files say so in their header; regenerate, do not hand-edit.

## Refreshing

```
scripts/refresh-gvisor-tests.py --dry-run [GVISOR_COMMIT]   # report only
scripts/refresh-gvisor-tests.py GVISOR_COMMIT               # apply
scripts/refresh-gvisor-tests.py --check                     # drift guard
```

A refresh is an ordinary hermit pull request. In an fbsource checkout
this directory is import output, so only `--check`/`--dry-run` are
meaningful there; apply refreshes on the GitHub side.

## Building

Buck only. Cargo does not build this C++ corpus, and nothing here is an
input to any Rust crate; `cargo build`/`cargo test` are unaffected.

Dependencies (googletest/gmock, abseil, benchmark) resolve to fbsource
third-party targets in fbcode, and to `shim//third-party/...` system
libraries in the open-source Buck build. Inside the hermetic validation
image they must come from the image itself (`ci/hermetic/flake.nix`);
validation nodes do not fetch from the network. The vendored code is
compiled as C++17, matching upstream's own build flags, so no local
patches to upstream files are needed.
