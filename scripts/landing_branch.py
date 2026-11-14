# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Read one branch setting of ci/landing-branches.env.

The environment variable named after the key wins when it is set; otherwise
the key's KEY=value line in the checkout's ci/landing-branches.env. A setting
that is missing, empty, repeated or not a plain branch name raises ValueError:
there is no default branch to fall back to. scripts/lib/landing_branch.sh and
ci/manifest-plan/src/landing_branch.rs implement the same rule.
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Mapping

SETTINGS_FILE = "ci/landing-branches.env"
HERMIT_LANDING_BRANCH = "HERMIT_LANDING_BRANCH"
REVERIE_LANDING_BRANCH = "REVERIE_LANDING_BRANCH"
ROOT = Path(__file__).resolve().parents[1]

_KEY = re.compile(r"[A-Z][A-Z0-9_]*")
_COMPONENT = r"[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9_-])?"
_BRANCH = re.compile(rf"{_COMPONENT}(?:/{_COMPONENT})*")


def is_branch_name(name: str) -> bool:
    """Whether NAME is a plain branch name.

    '/'-separated components, each starting with an ASCII letter or digit,
    made of letters, digits, '.', '_' and '-', and not ending in '.' or
    '.lock'; no '..' anywhere.
    """
    return (
        _BRANCH.fullmatch(name) is not None
        and ".." not in name
        and not any(part.endswith(".lock") for part in name.split("/"))
    )


def value_in(text: str, key: str) -> str | None:
    """The value TEXT, the settings file, gives KEY; None when no line sets it.

    Every line is blank, a '#' comment, or exactly KEY=value with no
    surrounding space; anything else, and a key set twice, is an error.
    """
    found = None
    for number, raw in enumerate(text.split("\n"), 1):
        line = raw[:-1] if raw.endswith("\r") else raw
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        name, separator, value = line.partition("=")
        if not separator or _KEY.fullmatch(name) is None:
            raise ValueError(f"{SETTINGS_FILE}:{number} is not a KEY=value line")
        if name != key:
            continue
        if found is not None:
            raise ValueError(f"{SETTINGS_FILE} sets {key} more than once")
        found = value
    return found


def landing_branch(
    key: str, root: Path = ROOT, environ: Mapping[str, str] = os.environ
) -> str:
    """The branch KEY names: ENVIRON[KEY] when set, else ROOT's settings file."""
    if key in environ:
        source, value = f"environment variable {key}", environ[key]
    else:
        path = root / SETTINGS_FILE
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as error:
            raise ValueError(
                f"cannot read {path}, and {key} is not set: {error}"
            ) from error
        found = value_in(text, key)
        if found is None:
            raise ValueError(f"{path} does not set {key}")
        source, value = str(path), found
    if not is_branch_name(value):
        raise ValueError(
            f"{source} gives {key} the value {value!r}, which is not a branch name"
        )
    return value
