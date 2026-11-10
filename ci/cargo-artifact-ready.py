#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Forward Cargo JSON diagnostics and signal when the Hermit binary is ready.

The pinned workspace compile runs its unified Cargo invocation through this
filter. The signal is published only after Cargo itself reports all of the
artifacts the early E2E publisher snapshots: the hermit-install build
script, the stage-detcore-liteinst binary, and the final Hermit binary.
Signalling uses dagrun's file protocol directly so it works inside the
pinned root, where the ready directory is bind-mounted at the path named by
DAGRUN_ARTIFACT_READY_DIR.
"""

import json
import os
import sys
from pathlib import Path

ARTIFACT = "hermit-bin"


def signal_ready() -> bool:
    ready_dir = os.environ.get("DAGRUN_ARTIFACT_READY_DIR")
    step = os.environ.get("DAGRUN_OUTER_RUN")
    if not ready_dir or not step:
        print(
            "cargo-artifact-ready: DAGRUN_ARTIFACT_READY_DIR or "
            "DAGRUN_OUTER_RUN is unset; cannot signal",
            file=sys.stderr,
            flush=True,
        )
        return False
    step_dir = Path(ready_dir) / step
    step_dir.mkdir(parents=True, exist_ok=True)
    target = step_dir / ARTIFACT
    temporary = step_dir / f".{ARTIFACT}.tmp-{os.getpid()}"
    temporary.write_bytes(b"")
    os.replace(temporary, target)
    print(f"cargo-artifact-ready: signalled {ARTIFACT}", file=sys.stderr, flush=True)
    return True


def is_final_executable(event: dict, target_name: str, suffix: str) -> bool:
    target = event.get("target") or {}
    profile = event.get("profile") or {}
    executable = event.get("executable") or ""
    return (
        target.get("name") == target_name
        and not profile.get("test", False)
        and executable.endswith(suffix)
    )


def main() -> int:
    if len(sys.argv) != 2 or sys.argv[1] != ARTIFACT:
        print(f"usage: {sys.argv[0]} {ARTIFACT}", file=sys.stderr)
        return 2

    predicates = set()
    signalled = False
    for raw_line in sys.stdin.buffer:
        line = raw_line.decode("utf-8", errors="replace")
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            sys.stdout.write(line)
            sys.stdout.flush()
            continue
        if not isinstance(event, dict):
            sys.stdout.write(line)
            sys.stdout.flush()
            continue

        reason = event.get("reason")
        if reason == "compiler-message":
            rendered = (event.get("message") or {}).get("rendered")
            if rendered:
                sys.stdout.write(rendered)
                if not rendered.endswith("\n"):
                    sys.stdout.write("\n")
                sys.stdout.flush()
        elif reason == "compiler-artifact":
            target = event.get("target") or {}
            if not event.get("fresh", False) and target.get("name"):
                print(f"   Compiling {target['name']}", flush=True)
            if is_final_executable(event, "hermit", "/target/validate/hermit"):
                predicates.add("hermit-bin")
            if is_final_executable(
                event,
                "stage-detcore-liteinst",
                "/target/validate/stage-detcore-liteinst",
            ):
                predicates.add("stage-detcore-liteinst")
        elif reason == "build-script-executed":
            if "hermit-install" in str(event.get("package_id", "")):
                predicates.add("hermit-install")

        if not signalled and predicates == {
            "hermit-bin",
            "hermit-install",
            "stage-detcore-liteinst",
        }:
            signalled = signal_ready()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
