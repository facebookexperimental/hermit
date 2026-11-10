"""Evaluate a Bazel BUILD file as restricted Python and collect rule calls.

gVisor's BUILD files under test/ use a tiny Starlark subset (lists, strings,
dicts, `+`, function calls). We evaluate them with Python's exec() against a
namespace of stub rule functions that only record their keyword arguments.
`select()` is resolved eagerly for one configuration (default: linux/amd64).
Anything unknown raises, so an upstream change that introduces a new macro is
reported rather than silently ignored.
"""

import os

ARCH_KEYS = {
    "amd64": ("@platforms//cpu:x86_64", "//tools:amd64", "amd64"),
}


class Rule:
    def __init__(self, kind, pkg, kwargs):
        self.kind = kind
        self.pkg = pkg
        self.name = kwargs.get("name")
        self.kw = kwargs

    @property
    def label(self):
        return "//%s:%s" % (self.pkg, self.name)


def _flatten_select(d, arch):
    # d: dict of condition label -> value. Pick by our configuration.
    for k, v in d.items():
        if arch == "amd64" and ("amd64" in k or "x86_64" in k):
            return v
    for k, v in d.items():
        if "darwin" in k or "arm64" in k or "aarch64" in k or "riscv64" in k:
            continue
        if k == "//conditions:default":
            return v
    raise ValueError("select() has no branch for %s: %r" % (arch, list(d)))


def eval_build(path, pkg, arch="amd64"):
    rules = []

    def rec(kind):
        def f(**kw):
            rules.append(Rule(kind, pkg, kw))
        return f

    def select(d, no_match_error=None):
        return _flatten_select(d, arch)

    def select_arch(amd64=None, arm64=None, riscv64=None, default=None, **kw):
        val = {"amd64": amd64, "arm64": arm64, "riscv64": riscv64}[arch]
        if val is None:
            val = default
        if val is None:
            if "no_match_error" in kw:
                raise ValueError(kw["no_match_error"])
            return []
        return val

    def select_system(linux=["__linux__"], darwin=[], **_kw):
        return linux

    ns = {
        "load": lambda *a, **k: None,
        "package": lambda **k: None,
        "exports_files": lambda *a, **k: None,
        "licenses": lambda *a, **k: None,
        "select": select,
        "select_arch": select_arch,
        "select_system": select_system,
        "select_gtest": lambda: ["@com_google_googletest//:gtest"],
        "gbenchmark": "@com_google_benchmark//:benchmark",
        "gbenchmark_internal": "@com_google_benchmark//:benchmark",
        "coreutil": lambda: [],
        "default_net_util": lambda: [],
        "cc_toolchain": "@bazel_tools//tools/cpp:current_cc_toolchain",
        "glob": lambda *a, **k: [],
        "True": True,
        "False": False,
        "None": None,
    }
    for kind in ("cc_binary", "cc_library", "cc_test", "sh_binary", "genrule",
                 "cc_flags_supplier", "filegroup", "go_binary", "go_library"):
        ns[kind] = rec(kind)
    with open(path) as fh:
        src = fh.read()
    code = compile(src, path, "exec")
    exec(code, {"__builtins__": {}}, ns)
    return rules


def resolve_label(label, cur_pkg):
    """Normalise ':x', '//p:x', '//p' (-> //p:basename(p)) to (repo, pkg, name)."""
    repo = ""
    if label.startswith("@"):
        repo, _, label = label.partition("//")
        label = "//" + label
    if label.startswith(":"):
        return repo, cur_pkg, label[1:]
    if label.startswith("//"):
        body = label[2:]
        if ":" in body:
            p, n = body.split(":", 1)
        else:
            p, n = body, os.path.basename(body)
        return repo, p, n
    # bare file name
    return repo, cur_pkg, label
