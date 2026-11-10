# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

def _filter_empty_strings(string_list):
    return filter(lambda d: d != "", string_list)

def _parse_prefix_mappings(raw_rules):
    rules = []
    for raw_rule in raw_rules:
        (match, replace) = raw_rule.split("->", 1)

        (cell, root_dir) = match.split("//")
        match = struct(cell = cell, root_dir = root_dir)

        (cell, root_dir) = replace.split("//")
        replace = struct(cell = cell, root_dir = root_dir)

        rules.append(struct(match = match, replace = replace))

    return rules

# Cell the BUCK file being processed belongs to
ACTIVE_CELL = native.get_cell_name()

# The [oss] settings below live in the root cell's .buckconfig. They are read
# from there so that BUCK files in other cells of the project (a submodule
# mapped through prefix_mappings, which has no .buckconfig of its own) get the
# same rewrites.

# The root cell of this project, extracted from the "root" alias.
# Targets that explicitly reference this cell will not be rewritten, and
# targets that do not end up referencing a cell will be replaced with targets
# that reference this cell
ROOT_CELL = read_config("cell_aliases", "root", "root")

# The cell this file and the rest of the shim directory belong to, generally
# "shim" and does not need to be set.
SHIM_CELL = read_root_config("oss", "shim_cell", "shim")

# The internal cell this project originally belonged to.
#
# When applying rewrites, the cell of the target is often considered. Targets
# that do not explicitly specify a cell (eg: "//foo:bar") will be considered
# to belong to INTERNAL_CELL.
INTERNAL_CELL = read_root_config("oss", "internal_cell", "fbcode")

# There can be situations where a target specifies a cell explicitly and the
# path is part of the local checkout, rather than potentially needing to be
# shimmed. In this case, we want to rewrite the target to use the root cell.
#
# If a target's cell is unspecified or matches the internal cell, and the path
# starts with an entry in this list, The cell replaced with the ROOT_CELL.
#
# Entries are separated by spaces, and evaluated in order. Once a match is
# found, the rewrite is complete and the following entries will not be
# evaluated.
#
# Examples:
#     internal_cell//oss_project/foo:bar -> root//oss_project/foo:bar
PROJECT_DIRS = _filter_empty_strings(read_root_config("oss", "project_dirs", "").split(" "))

# There are some situations where prefix of the internal directory structure is
# removed from the public filepaths, such as rewriting "internal/foo/bar/baz"
# to "oss/baz". When this happens, the BUCK files are not converted to reflect
# the public directory structure, and targets need to be rewritten to account
# for the discrepancy.
#
# Entries behave similarly to PROJECT_DIRS, except that the root directory will
# also be removed from the path in the rewritten target. This setting is
# applied after PROJECT_DIRS.
#
# Entries are separated by spaces and evaluated in order. Once a match is
# found, the rewrite is complete and the following entries will not be
# evaluated.
#
# Examples:
#     //oss_project/foo:bar -> root//foo:bar
#     internal_cell//oss_project/foo:bar -> root//foo:bar
STRIPPED_ROOT_DIRS = _filter_empty_strings(read_root_config("oss", "stripped_root_dirs", "").split(" "))

# Internally, most code shares the same cell in a monorepo, but public projects
# only contain a subset, importing dependencies via git submodules or other
# mechanisms. When this happens, the dependency may end up in a different
# filepath, or may have it's own buck2 configuration and should be treated as
# an on disk external cell.
#
# If the target's cell is a match (or if unspecified, INTERNAL_CELL is a
# match),unspecified) matches, and the target's path is within the root
# directory, both the cell and root directory prefix are replaced with the new
# values.
#
# Entries are in the form of "MATCH->REPLACEMENT". Both MATCH and replacement
# shall be in the format of "CELL//DIR_PREFIX".
#
# Entries are separated by spaces and evaluated in order. Once a match is
# found, the rewrite is complete and the following entries will not be
# evaluated.
#
# Examples:
#     internal//foo->foo//foo; internal//foo/bar:baz -> foo//foo/bar:baz
PREFIX_MAPPINGS = _parse_prefix_mappings(
    _filter_empty_strings(read_root_config("oss", "prefix_mappings", "").split(" ")),
)

# Hardcoded rewrite rules that apply to many projects and only produce targets
# within the shim cell. An `exact` rule renames one target and is applied
# first, before the project directories and the rules from .buckconfig. The
# `dirs` and `dynamic` rules are applied after those, and only if no other
# rule matches.
IMPLICIT_REWRITE_RULES = {
    "fbcode": struct(
        exact = {
            "common/rust/shed/fbinit:fbinit": "third-party/rust:fbinit",
            "common/rust/shed/sorted_vector_map:sorted_vector_map": "third-party/rust:sorted_vector_map",
            # fbsource builds agent-utils' dagrun as first-party code under
            # fb-only/, which is never exported; this tree takes the dagrun
            # crate (ci/manifest-plan/Cargo.toml). The rule is exact, so it
            # wins over the project directory rule for hermetic_infra/hermit.
            # scripts/check-buck-graph-drift.rs reads this entry.
            "hermetic_infra/hermit/fb-only/dagrun:dagrun": "third-party/rust:dagrun",
            "watchman/rust/watchman_client:watchman_client": "third-party/rust:watchman_client",
        },
        dirs = [
            ("third-party-buck/platform010/build/supercaml", "third-party/ocaml"),
            ("third-party-buck/platform010/build", "third-party"),
        ],
    ),
    "fbsource": struct(
        # Internal names that carry a major version because fbsource vendors
        # several majors of the crate. This tree's Cargo.lock has one, and
        # Reindeer names its alias without the suffix.
        # scripts/check-buck-graph-drift.rs reads these entries.
        exact = {
            "third-party/rust:sha2-010": "third-party/rust:sha2",
        },
        dirs = [
            ("third-party", "third-party"),
        ],
    ),
    "third-party": struct(
        dirs = [
            ("", "third-party"),
        ],
    ),
}

DEFAULT_REWRITE_CTX = struct(
    cells = struct(
        active = ACTIVE_CELL,
        root = ROOT_CELL,
        shim = SHIM_CELL,
        internal = INTERNAL_CELL,
    ),
    project_dirs = PROJECT_DIRS,
    stripped_root_dirs = STRIPPED_ROOT_DIRS,
    prefix_mappings = PREFIX_MAPPINGS,
    implicit_rewrite_rules = IMPLICIT_REWRITE_RULES,
)

"""
Rewrite an internal target string to one that is compatible with this OSS
project.

Some example use cases for this:
- Map dependency targets to shim targets in this dir
- Handle mismatching buck roots between internal and oss
  (eg: internal/oss-project/... is exposed externally as oss-project/...)
- Handle submodules that result in filepaths that do not match internal
  (eg: internal/my_library/... and oss-project/my_library/my_library/...)
"""

def translate_target(target: str, ctx = DEFAULT_REWRITE_CTX) -> str:
    if "//" not in target:
        # This is a local target, aka ":foo". Don't touch
        return target

    (cell, path) = target.split("//", 1)

    if cell == ctx.cells.root:
        # This cell is explicitly root. Don't touch
        return target

    resolved_cell = ctx.cells.active if cell == "" else cell

    # A cell-relative label in the root cell, or in a cell that a prefix
    # mapping maps an internal directory onto, was written in the internal
    # cell: an internal BUCK file under a mapped directory names its own
    # package by its internal path (//oss_project/dep:foo in dep's BUCK file).
    if resolved_cell == ctx.cells.root or (cell == "" and _is_mapped_cell(resolved_cell, ctx)):
        internal_cell = ctx.cells.internal
    else:
        internal_cell = resolved_cell

    rules = ctx.implicit_rewrite_rules.get(internal_cell)

    # An exact rule names one internal target, so it wins over every directory
    # rule below, including the project's own directories.
    exact = getattr(rules, "exact", {}).get(path) if rules != None else None
    if exact != None:
        return ctx.cells.shim + "//" + exact

    if internal_cell == ctx.cells.internal:
        for d in ctx.project_dirs:
            if _path_rooted_in_dir(path, d):
                return ctx.cells.root + "//" + path

        for d in ctx.stripped_root_dirs:
            if _path_rooted_in_dir(path, d):
                return ctx.cells.root + "//" + _strip_root_dir_from_path(path, d)

    for rule in ctx.prefix_mappings:
        if internal_cell == rule.match.cell and _path_rooted_in_dir(path, rule.match.root_dir):
            return rule.replace.cell + "//" + _swap_root_dir_for_path(path, rule.match.root_dir, rule.replace.root_dir)

    if rules == None:
        # No implicit rewrite rules
        return target

    for match_root_dir, replace_root_dir in getattr(rules, "dirs", []):
        if _path_rooted_in_dir(path, match_root_dir):
            return ctx.cells.shim + "//" + _swap_root_dir_for_path(path, match_root_dir, replace_root_dir)

    for match_root_dir, rewrite in getattr(rules, "dynamic", []):
        if _path_rooted_in_dir(path, match_root_dir):
            return ctx.cells.shim + "//" + rewrite(path)

    return target

# String-parameter macros whose argument is one target label. Internal BUCK
# files name targets by internal path inside these macros too (for example
# "$(location //oss_project/foo:bar)" in a test's args or env).
_LABEL_MACROS = ["exe", "exe_target", "location", "location_exec", "locations"]

"""
Rewrite the target labels inside the string-parameter macros of `s` (each
`$(MACRO LABEL)` whose MACRO is in _LABEL_MACROS) as translate_target rewrites
a label. Text outside those macros, and every other macro, is unchanged.
"""

def translate_macros(s: str, ctx = DEFAULT_REWRITE_CTX) -> str:
    out = ""
    rest = s

    # Each pass consumes at least one character, so len(s) passes suffice.
    for _ in range(len(s)):
        start = rest.find("$(")
        if start < 0:
            break
        end = rest.find(")", start)
        if end < 0:
            break
        out += rest[:start]
        parts = rest[start + 2:end].split(" ", 1)
        if len(parts) == 2 and parts[0] in _LABEL_MACROS:
            out += "$(" + parts[0] + " " + translate_target(parts[1].strip(), ctx) + ")"
        else:
            out += rest[start:end + 1]
        rest = rest[end + 1:]
    return out + rest

def _is_mapped_cell(cell: str, ctx) -> bool:
    for rule in ctx.prefix_mappings:
        if rule.match.cell == ctx.cells.internal and rule.replace.cell == cell:
            return True
    return False

def _path_rooted_in_dir(path: str, d: str) -> bool:
    return d == "" or path == d or path.startswith(d + "/") or path.startswith(d + ":")

def _strip_root_dir_from_path(path: str, d: str) -> str:
    return path.removeprefix(d).removeprefix("/")

def _swap_root_dir_for_path(path: str, root_dir: str, new_root_dir) -> str:
    suffix = _strip_root_dir_from_path(path, root_dir)
    if not suffix.startswith(":"):
        suffix = "/" + suffix
    replace_path = new_root_dir.removesuffix("/") + suffix
    return replace_path.removeprefix("/")
