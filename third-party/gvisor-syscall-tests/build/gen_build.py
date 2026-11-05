#!/usr/bin/env python3
"""Generate CMake build rules for gVisor's syscall tests from gVisor's BUILD files.

Reads (from the gVisor checkout, unmodified):
    test/util/BUILD
    test/syscalls/linux/BUILD
    test/syscalls/linux/rseq/BUILD

Writes:
    generated/targets.json       build-system-neutral description (the IR)
    generated/gvisor_tests.cmake CMake rules consumed by the top-level CMakeLists.txt
    generated/combined_manifest.inc  C++ table used by the combined binary's dispatcher

The IR is the contract: a Buck2 (or anything else) emitter only has to read
targets.json. See emit_cmake() for the one emitter that exists today.

Usage: gen/gen_build.py [--gvisor third_party/gvisor] [--out generated] [--check]
  --check  regenerate in memory and exit 1 if the checked-in output differs.
"""

import argparse
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from starlark_eval import eval_build, resolve_label  # noqa: E402

PACKAGES = ["test/util", "test/syscalls/linux", "test/syscalls/linux/rseq"]

# Bazel label -> CMake target, for the external C++ dependencies. Anything not
# listed falls back to absl::<name>, and CMake fails loudly at configure time
# if that guess is wrong, so a new upstream absl dep cannot be silently dropped.
ABSL_NAME_MAP = {
    ("absl/flags", "flag"): "absl::flags",
    ("absl/flags", "parse"): "absl::flags_parse",
    ("absl/algorithm", "container"): "absl::algorithm_container",
    ("absl/random", "random"): "absl::random_random",
    ("absl/log", "log"): "absl::log",
    ("absl/log", "check"): "absl::check",
}
EXTERNAL = {
    "@com_google_googletest//:gtest": ["GTest::gmock", "GTest::gtest"],
    "@com_google_googletest//:gtest_main": ["GTest::gtest_main"],
    "@com_google_benchmark//:benchmark": ["benchmark::benchmark"],
    # Bazel's runfiles library: replaced by support/runfiles_shim.cc.
    "@bazel_tools//tools/cpp/runfiles": [],
}
# Sources replaced by scaffolding (never edited in place).
SOURCE_OVERRIDES = {
    "test/util/test_util_runfiles.cc": "support/runfiles_shim.cc",
}
# Targets whose output we know we cannot build without Bazel's Go toolchain.
UNSUPPORTED_DATA = {
    "//test/cmd/test_app:test_app": "Go binary (rules_go); not built by this repo",
}
TEST_MAIN = "//test/util:test_main"
GTEST = "@com_google_googletest//:gtest"


def absl_target(label):
    _repo, pkg, name = resolve_label(label, "")
    return ABSL_NAME_MAP.get((pkg, name), "absl::" + name)


def ident(s):
    return re.sub(r"[^A-Za-z0-9_]", "_", s)


def load_rules(gvisor):
    rules = {}
    for pkg in PACKAGES:
        for r in eval_build(os.path.join(gvisor, pkg, "BUILD"), pkg):
            rules[r.label] = r
    return rules


def norm(label, pkg):
    repo, p, n = resolve_label(label, pkg)
    if repo:
        return label  # external labels stay verbatim
    return "//%s:%s" % (p, n)


def src_paths(rule, files_kind=(".cc", ".S", ".c")):
    out = []
    for s in rule.kw.get("srcs", []):
        if s.startswith(":") or s.startswith("//"):
            raise ValueError("%s: label in srcs not supported: %s" % (rule.label, s))
        p = os.path.join(rule.pkg, s)
        if p.endswith(files_kind):
            out.append(SOURCE_OVERRIDES.get(p, p))
    return out


MAIN_RE = re.compile(r"^int\s+main\s*\(", re.M)
FLAG_RE = re.compile(r"^ABSL_FLAG\(\s*([^,]+?)\s*,\s*(\w+)\s*,\s*([^,]+?)\s*,", re.M)
SUITE_RE = re.compile(r"^\s*(TEST|TEST_F|TEST_P|TYPED_TEST|TYPED_TEST_P)\(\s*(\w+)\s*,", re.M)
INST_RE = re.compile(r"INSTANTIATE_(?:TYPED_)?TEST_SUITE_P\(\s*(\w+)\s*,\s*(\w+)")


def build_ir(gvisor, allowlist=None):
    rules = load_rules(gvisor)
    libs, bins, helpers, warnings = {}, {}, {}, []

    def ext_or_internal(deps, pkg):
        internal, external = [], []
        for d in deps:
            if d.startswith("@"):
                if d in EXTERNAL:
                    external += EXTERNAL[d]
                elif d.startswith("@com_google_absl//"):
                    external.append(absl_target(d))
                else:
                    raise ValueError("unknown external dep %s" % d)
            else:
                internal.append(norm(d, pkg))
        return internal, external

    for label, r in sorted(rules.items()):
        if r.kind == "cc_library":
            internal, external = ext_or_internal(r.kw.get("deps", []), r.pkg)
            libs[label] = {
                "name": r.name,
                "pkg": r.pkg,
                "srcs": src_paths(r),
                "deps": internal,
                "external": external,
                "defines": list(r.kw.get("defines", [])),
                "alwayslink": bool(r.kw.get("alwayslink", False)),
            }

    for label, r in sorted(rules.items()):
        if r.pkg != "test/syscalls/linux":
            continue
        if r.kind == "cc_binary":
            deps = list(r.kw.get("deps", []))
            internal, external = ext_or_internal(deps, r.pkg)
            malloc = r.kw.get("malloc")
            entry = {
                "name": r.name,
                "srcs": src_paths(r),
                "deps": [d for d in internal if d != TEST_MAIN],
                "uses_test_main": TEST_MAIN in internal,
                "external": external,
                "malloc": norm(malloc, r.pkg) if malloc else None,
                "data": [norm(d, r.pkg) for d in r.kw.get("data", [])],
                "defines": list(r.kw.get("defines", [])),
                "copts": list(r.kw.get("copts", [])),
                "linkopts": list(r.kw.get("linkopts", [])),
            }
            if GTEST in deps and not entry["srcs"]:
                # e.g. pointer_auth_test is arm64-only: select_arch(default=[]).
                warnings.append("%s has no sources for this arch; skipped (Bazel would "
                                "link an empty test binary)" % r.name)
            elif GTEST in deps:
                bins[label] = entry
            else:
                helpers[label] = dict(entry, kind="cc_binary")
        elif r.kind == "sh_binary":
            helpers[label] = {"name": r.name, "kind": "sh_binary",
                              "srcs": [os.path.join(r.pkg, s) for s in r.kw["srcs"]]}
    if allowlist is not None:
        # Vendor only what we run: keep allowlisted tests and the helpers
        # they name as data deps. Libraries stay parsed; emitters already
        # restrict to the used closure, and the source scans below are
        # restricted to kept files so a partial vendoring still checks.
        missing = sorted(set(allowlist) - {b["name"] for b in bins.values()})
        if missing:
            raise ValueError("allowlisted targets not found upstream: %s" % missing)
        bins = {k: v for k, v in bins.items() if v["name"] in allowlist}
        keep_helpers = {d for b in bins.values() for d in b["data"]}
        helpers = {k: v for k, v in helpers.items() if k in keep_helpers}

    # The rseq genrule is the only genrule a test depends on; emit it by hand.
    rseq = rules.get("//test/syscalls/linux/rseq:rseq_binary")
    if rseq is not None and (allowlist is None
                             or "//test/syscalls/linux/rseq:rseq" in keep_helpers):
        helpers["//test/syscalls/linux/rseq:rseq"] = {
            "name": "rseq", "kind": "rseq_genrule", "pkg": "test/syscalls/linux/rseq",
            "srcs": ["test/syscalls/linux/rseq/critical_amd64.S",
                     "test/syscalls/linux/rseq/start_amd64.S",
                     "test/syscalls/linux/rseq/rseq.cc"],
        }

    # Transitive library closure per binary, and per-source facts.
    def closure(dlabels, seen=None):
        seen = [] if seen is None else seen
        for d in dlabels:
            if d in seen:
                continue
            if d not in libs:
                raise ValueError("dependency %s is not a cc_library we parsed" % d)
            seen.append(d)
            closure(libs[d]["deps"], seen)
        return seen

    src_users = {}
    for label, b in bins.items():
        for s in b["srcs"]:
            src_users.setdefault(s, []).append(b["name"])
    lib_srcs = {s for l in libs.values() for s in l["srcs"]}
    for s in src_users:
        if s in lib_srcs:
            raise ValueError("%s is both a library source and a binary source" % s)

    for label, b in bins.items():
        cl = closure(b["deps"] + ([b["malloc"]] if b["malloc"] else []))
        b["lib_closure"] = cl
        mains, prefixes = [], set()
        files = list(b["srcs"]) + [s for l in cl for s in libs[l]["srcs"]]
        for s in files:
            path = os.path.join(gvisor, s) if not s.startswith("support/") else None
            if path is None:
                continue
            text = open(path).read()
            if s in b["srcs"] and MAIN_RE.search(text):
                mains.append(s)
            for m in INST_RE.finditer(text):
                prefixes.add(m.group(1))
        b["main_srcs"] = mains
        b["inst_prefixes"] = sorted(prefixes)
        b["test_files"] = sorted(set(f for f in files if f.startswith("test/syscalls/linux/")))
        if len(mains) > 1:
            raise ValueError("%s: more than one main(): %s" % (b["name"], mains))
        if mains and b["uses_test_main"]:
            # Bazel links test_main as an archive; the binary's own main wins.
            warnings.append("%s defines main() and also depends on test_main; "
                            "own main wins (as under Bazel)" % b["name"])
        for d in b["data"]:
            if d in UNSUPPORTED_DATA:
                warnings.append("%s: data dep %s skipped: %s" % (b["name"], d, UNSUPPORTED_DATA[d]))
            elif d not in helpers:
                raise ValueError("%s: unknown data dep %s" % (b["name"], d))

    shared = sorted(s for s, u in src_users.items() if len(u) > 1)

    # absl flags defined in more than one test source collide in the combined
    # binary twice over: duplicate FLAGS_<name> symbols at link time, and a
    # fatal duplicate registration in absl's flag registry at startup. When
    # every definition has the same type and default (true for all of them at
    # the pinned commit), the first source keeps its definition and the others
    # are recompiled, for the combined binary only, with ABSL_FLAG(...) for
    # that name turned into ABSL_DECLARE_FLAG(...). Differing definitions are
    # a hard error; they would need a real rename.
    flag_defs = {}
    for src in sorted(src_users):
        text = open(os.path.join(gvisor, src)).read()
        for m in FLAG_RE.finditer(text):
            flag_defs.setdefault(m.group(2), []).append(
                {"src": src, "type": m.group(1).strip(), "default": m.group(3).strip()})
    flag_share = {}  # non-owner src -> {"declare": [...], "define": [...]}
    collisions = {}
    for name, defs in sorted(flag_defs.items()):
        if len(defs) < 2:
            continue
        collisions[name] = defs
        sig = {(d["type"], d["default"]) for d in defs}
        if len(sig) != 1:
            raise ValueError("flag %s is defined differently in %s; needs a rename"
                             % (name, [d["src"] for d in defs]))
        for d in defs[1:]:
            flag_share.setdefault(d["src"], {"declare": [], "define": []})["declare"].append(name)
    for src, info in flag_share.items():
        text = open(os.path.join(gvisor, src)).read()
        info["define"] = sorted({m.group(2) for m in FLAG_RE.finditer(text)} - set(info["declare"]))

    # gtest suites: two different fixture types under one suite name abort at
    # startup ("Attempted redefinition of test suite"). That never happens in
    # one upstream binary, but does once every file shares a process. Each
    # file gets a type key: its own fixture (class defined in the .cc), plain
    # TEST (::testing::Test), or a fixture from a header. When one suite name
    # has several keys, the files of every key but the first are recompiled
    # for the combined binary with -D<Suite>=<Suite>__<file stem>.
    if allowlist is not None:
        kept_libs = {l for b in bins.values() for l in b["lib_closure"]}
        scan_lib_srcs = {x for l in kept_libs for x in libs[l]["srcs"]
                         if x.startswith("test/syscalls/linux/")}
    else:
        scan_lib_srcs = {x for l in libs.values() for x in l["srcs"]
                         if x.startswith("test/syscalls/linux/")}
    suite_keys = {}
    for src in sorted(set(src_users) | scan_lib_srcs):
        text = open(os.path.join(gvisor, src)).read()
        for m in SUITE_RE.finditer(text):
            kind, suite = m.group(1), m.group(2)
            if re.search(r"(?:class|struct)\s+%s\b\s*[:{]|using\s+%s\s*=" % (suite, suite), text):
                key = "fixture in " + src
            elif kind == "TEST":
                key = "::testing::Test"
            else:
                key = "fixture from a header"
            suite_keys.setdefault(suite, {}).setdefault(key, set()).add(src)
    suite_renames = {}  # src -> {suite: new name}
    suite_collisions = {}
    for suite, keys in sorted(suite_keys.items()):
        if len(keys) < 2:
            continue
        order = sorted(keys, key=lambda k: (not k.startswith(("::", "fixture from")),
                                            any(f in lib_srcs for f in keys[k]) is False, k))
        suite_collisions[suite] = {k: sorted(v) for k, v in keys.items()}
        for k in order[1:]:
            for f in keys[k]:
                if f in lib_srcs:
                    raise ValueError("suite %s: would need to rename library source %s" % (suite, f))
                stem = os.path.splitext(os.path.basename(f))[0]
                suite_renames.setdefault(f, {})[suite] = "%s__%s" % (suite, stem)
    # Value-parameterized suites instantiated under the same prefix in several
    # files (e.g. AllUnixDomainSockets/AllSocketPairTest in bind.cc,
    # accept_bind.cc, accept_bind_stream.cc and socket_unix_domain.cc): in one
    # process every instantiation applies to every file's TEST_P patterns,
    # multiplying and duplicating test names. Files that hold both the
    # patterns and the instantiation get their own suite name, an alias of
    # the shared header fixture, so each instantiation only sees its own file.
    inst_files = {}
    for src in sorted(src_users):
        text = open(os.path.join(gvisor, src)).read()
        for m in INST_RE.finditer(text):
            inst_files.setdefault((m.group(1), m.group(2)), []).append(src)
    suite_aliases = {}  # src -> {suite: (new name, header)}
    inst_collisions = {}
    for (prefix, suite), files in sorted(inst_files.items()):
        if len(files) < 2:
            continue
        inst_collisions["%s/%s" % (prefix, suite)] = files
        if suite in suite_collisions:
            continue  # already separated by the fixture-type renames above
        for f in files:
            text = open(os.path.join(gvisor, f)).read()
            if f in suite_renames and suite in suite_renames[f]:
                continue  # already separated by the fixture-type rename above
            if not re.search(r"TEST_P\(\s*%s\s*," % suite, text):
                continue  # patterns live elsewhere (a library); keep the upstream name
            header = None
            for inc in re.findall(r'^#include "([^"]+\.h)"', text, re.M):
                hp = os.path.join(gvisor, inc)
                if os.path.exists(hp) and re.search(
                        r"(?:class|struct)\s+%s\b\s*[:{]|using\s+%s\s*=" % (suite, suite), open(hp).read()):
                    header = inc
                    break
            if header is None:
                raise ValueError("%s: cannot find the header defining fixture %s" % (f, suite))
            stem = os.path.splitext(os.path.basename(f))[0]
            suite_aliases.setdefault(f, {})[suite] = ("%s__%s" % (suite, stem), header)
    for f, al in suite_aliases.items():
        for suite, (new, _h) in al.items():
            suite_renames.setdefault(f, {})[suite] = new

    # gtest global environments are process-wide; gate them on the selected
    # target (support/gvt_env_gate.h).
    env_files = sorted(f for f in src_users
                       if "AddGlobalTestEnvironment(" in open(os.path.join(gvisor, f)).read())
    for f in (lib_srcs if allowlist is None else
              {x for l in kept_libs for x in libs[l]["srcs"]}):
        if f.startswith("test/") and "AddGlobalTestEnvironment(" in open(os.path.join(gvisor, f)).read():
            raise ValueError("library source %s registers a global environment; not handled" % f)

    combined_only = {}
    for f in set(flag_share) | set(suite_renames) | set(env_files):
        combined_only[f] = {"flags": flag_share.get(f), "suite_renames": suite_renames.get(f, {}),
                            "aliases": {k: list(v) for k, v in suite_aliases.get(f, {}).items()},
                            "gate_env": f in env_files}
    for b in bins.values():
        b["suite_renames"] = {o: n for f in b["srcs"] for o, n in suite_renames.get(f, {}).items()}
    return {
        "global_environment_files": env_files,
        "instantiation_collisions": inst_collisions,
        "suite_collisions": suite_collisions,
        "combined_only": combined_only,
        "flag_collisions": collisions,
        "flag_share": flag_share,
        "libraries": libs,
        "tests": bins,
        "helpers": helpers,
        "shared_test_srcs": {s: src_users[s] for s in shared},
        "warnings": warnings,
    }


# ----------------------------------------------------------------------------
# CMake emitter


def lib_target(label):
    return "gvl_" + ident(label.lstrip("/").replace(":", "_"))


def src_target(src):
    return "gvs_" + ident(os.path.splitext(os.path.basename(src))[0])


def emit_cmake(ir):
    L = []
    w = L.append
    w("# GENERATED by gen/gen_build.py from gVisor's BUILD files. Do not edit.")
    w("")
    libs = ir["libraries"]
    used = {TEST_MAIN}
    for b in ir["tests"].values():
        used.update(b["lib_closure"])
    for h in ir["helpers"].values():
        if h.get("kind") == "cc_binary":
            used.update(h["deps"])
            if h.get("malloc"):
                used.add(h["malloc"])
    # also libs reachable from used libs
    stack = list(used)
    while stack:
        l = stack.pop()
        for d in libs[l]["deps"]:
            if d not in used:
                used.add(d)
                stack.append(d)

    w("# ---- libraries (%d used of %d parsed)" % (len(used), len(libs)))
    for label in sorted(used):
        l = libs[label]
        t = lib_target(label)
        srcs = [s if s.startswith("support/") else "${GVISOR_ROOT}/" + s for s in l["srcs"]]
        srcs = ["${GVT_ROOT}/" + s if s.startswith("support/") else s for s in srcs]
        if srcs:
            w("add_library(%s STATIC %s)" % (t, " ".join(srcs)))
            vis = "PUBLIC"
        else:
            w("add_library(%s INTERFACE)" % t)
            vis = "INTERFACE"
        deps = []
        for d in l["deps"]:
            dt = lib_target(d)
            deps.append("$<LINK_LIBRARY:WHOLE_ARCHIVE,%s>" % dt if libs[d]["alwayslink"] and libs[d]["srcs"] else dt)
        deps += l["external"]
        deps.append("gvt_common")
        w("target_link_libraries(%s %s %s)" % (t, vis, " ".join(deps)))
        if l["defines"]:
            w("target_compile_definitions(%s %s %s)" % (t, vis, " ".join(l["defines"])))
    w("")

    def link_lib_ref(label):
        l = libs[label]
        t = lib_target(label)
        return "$<LINK_LIBRARY:WHOLE_ARCHIVE,%s>" % t if (l["alwayslink"] and l["srcs"]) else t

    # Per-source object libraries: one per test .cc so that the per-test
    # binaries and the combined binary share every compiled object.
    w("# ---- per-source object libraries")
    tests = ir["tests"]
    all_srcs = {}
    for b in tests.values():
        for s in b["srcs"]:
            all_srcs.setdefault(s, []).append(b)
    for s in sorted(all_srcs):
        users = all_srcs[s]
        t = src_target(s)
        w("add_library(%s OBJECT ${GVISOR_ROOT}/%s)" % (t, s))
        # Usage requirements (include dirs, defines) from the union of users' deps.
        deps = set()
        defines, copts = set(), set()
        for b in users:
            deps.update(link_lib_ref(d) for d in b["lib_closure"])
            deps.update(b["external"])
            defines.update(b["defines"])
            copts.update(b["copts"])
        w("target_link_libraries(%s PRIVATE gvt_common %s)" % (t, " ".join(sorted(deps))))
        if defines:
            w("target_compile_definitions(%s PRIVATE %s)" % (t, " ".join(sorted(defines))))
        if copts:
            w("target_compile_options(%s PRIVATE %s)" % (t, " ".join(sorted(copts))))
    w("")

    w("# ---- per-test binaries (%d)" % len(tests))
    for label in sorted(tests):
        b = tests[label]
        n = b["name"]
        objs = " ".join("$<TARGET_OBJECTS:%s>" % src_target(s) for s in b["srcs"])
        w("add_executable(%s %s)" % (n, objs))
        deps = []
        if b["uses_test_main"] and not b["main_srcs"]:
            deps.append(link_lib_ref(TEST_MAIN))
        if b["malloc"]:
            deps.append("$<LINK_LIBRARY:WHOLE_ARCHIVE,%s>" % lib_target(b["malloc"]))
        deps += [link_lib_ref(d) for d in b["deps"]]
        deps += b["external"]
        deps += b["linkopts"]
        w("target_link_libraries(%s PRIVATE gvt_common %s)" % (n, " ".join(deps)))
        w("set_target_properties(%s PROPERTIES RUNTIME_OUTPUT_DIRECTORY ${GVT_BIN_DIR} EXCLUDE_FROM_ALL ${GVT_PERTEST_EXCLUDE})" % n)
        data = [d for d in b["data"] if d in ir["helpers"]]
        if data:
            w("add_dependencies(%s %s)" % (n, " ".join("gvh_" + ir["helpers"][d]["name"] for d in data)))
        w("list(APPEND GVT_PERTEST_TARGETS %s)" % n)
    w("")

    w("# ---- helper binaries (runfiles data deps)")
    w("set(GVT_HELPER_TARGETS %s)" % " ".join("gvh_" + ir["helpers"][l]["name"] for l in sorted(ir["helpers"])))
    for label in sorted(ir["helpers"]):
        h = ir["helpers"][label]
        n = h["name"]
        pkg = label[2:].split(":")[0]
        out_dir = "${GVT_RUNFILES_DIR}/" + pkg
        if h["kind"] == "cc_binary":
            srcs = " ".join("${GVISOR_ROOT}/" + s for s in h["srcs"])
            w("add_executable(gvh_%s %s)" % (n, srcs))
            deps = [lib_target(d) for d in h["deps"]] + h["external"]
            w("target_link_libraries(gvh_%s PRIVATE gvt_common %s)" % (n, " ".join(deps)))
            w("set_target_properties(gvh_%s PROPERTIES OUTPUT_NAME %s RUNTIME_OUTPUT_DIRECTORY %s)" % (n, n, out_dir))
        elif h["kind"] == "sh_binary":
            src = h["srcs"][0]
            w("add_custom_command(OUTPUT %s/%s COMMAND ${CMAKE_COMMAND} -E make_directory %s "
              "COMMAND ${CMAKE_COMMAND} -E copy ${GVISOR_ROOT}/%s %s/%s DEPENDS ${GVISOR_ROOT}/%s)"
              % (out_dir, n, out_dir, src, out_dir, n, src))
            w("add_custom_target(gvh_%s DEPENDS %s/%s)" % (n, out_dir, n))
        elif h["kind"] == "rseq_genrule":
            # Mirrors the genrule cmd in test/syscalls/linux/rseq/BUILD (amd64 branch).
            srcs = " ".join("${GVISOR_ROOT}/" + s for s in h["srcs"])
            w("add_custom_command(OUTPUT %s/rseq COMMAND ${CMAKE_COMMAND} -E make_directory %s "
              "COMMAND ${CMAKE_CXX_COMPILER} -fno-pie -no-pie -I${GVISOR_ROOT}/test/syscalls/linux/rseq "
              "-I${GVISOR_ROOT} -Wall -Werror -O2 -std=c++17 -static -nostdlib -ffreestanding -fno-exceptions "
              "-o %s/rseq %s DEPENDS %s WORKING_DIRECTORY ${GVISOR_ROOT}/test/syscalls/linux/rseq)"
              % (out_dir, out_dir, out_dir, srcs, srcs))
            w("add_custom_target(gvh_rseq DEPENDS %s/rseq)" % out_dir)
    w("")

    # Combined binary: every test object once, plus the dispatcher.
    w("# ---- combined binary")
    w("# Sources recompiled for the combined binary only: shared absl flags turned")
    w("# into declarations, colliding gtest suites renamed (generated/combined_only/*.h).")
    for s in sorted(ir["combined_only"]):
        users = all_srcs[s]
        t = src_target(s)
        w("add_library(%s_c OBJECT ${GVISOR_ROOT}/%s)" % (t, s))
        w("target_link_libraries(%s_c PRIVATE $<TARGET_PROPERTY:%s,LINK_LIBRARIES>)" % (t, t))
        w("target_include_directories(%s_c PRIVATE ${GVT_ROOT})" % t)
        w("target_compile_definitions(%s_c PRIVATE $<TARGET_PROPERTY:%s,COMPILE_DEFINITIONS>)" % (t, t))
        w("target_compile_options(%s_c PRIVATE $<TARGET_PROPERTY:%s,COMPILE_OPTIONS> -include ${GVT_ROOT}/generated/combined_only/%s.h)"
          % (t, t, ident(s)))
    w("set(GVT_COMBINED_OBJECTS)")
    for s in sorted(all_srcs):
        owner = all_srcs[s][0]
        ot = src_target(s) + ("_c" if s in ir["combined_only"] else "")
        if s in owner["main_srcs"]:
            sym = "gvt_main_" + ident(owner["name"])
            out = "${CMAKE_BINARY_DIR}/renamed/%s.o" % ident(s)
            w("add_custom_command(OUTPUT %s COMMAND ${CMAKE_COMMAND} -E make_directory ${CMAKE_BINARY_DIR}/renamed "
              "COMMAND ${CMAKE_OBJCOPY} --redefine-sym main=%s $<TARGET_OBJECTS:%s> %s "
              "DEPENDS %s $<TARGET_OBJECTS:%s> COMMAND_EXPAND_LISTS)"
              % (out, sym, ot, out, ot, ot))
            w("list(APPEND GVT_COMBINED_OBJECTS %s)" % out)
        else:
            w("list(APPEND GVT_COMBINED_OBJECTS $<TARGET_OBJECTS:%s>)" % ot)
    deps = set()
    for b in tests.values():
        deps.update(link_lib_ref(d) for d in b["lib_closure"] if d != TEST_MAIN)
        if b["malloc"]:
            deps.add("$<LINK_LIBRARY:WHOLE_ARCHIVE,%s>" % lib_target(b["malloc"]))
        deps.update(b["external"])
        deps.update(b["linkopts"])
    w("set(GVT_COMBINED_LINK %s)" % " ".join(sorted(deps)))
    w("")
    return "\n".join(L) + "\n"


def emit_combined_only(src, info):
    L = ["// GENERATED by gen/gen_build.py. Do not edit.",
         "// Force-included when compiling %s for the combined binary only." % src]
    if info.get("gate_env"):
        L.append('#include "support/gvt_env_gate.h"  // gtest global environment runs only for its target')
    for old_name, new_name in sorted(info["suite_renames"].items()):
        if old_name in info.get("aliases", {}):
            _n, header = info["aliases"][old_name]
            L.append("// %s is instantiated under the same prefix in other files; give this" % old_name)
            L.append("// file's TEST_P patterns their own suite (an alias of the same fixture).")
            L.append('#include "%s"' % header)
            L.append("namespace gvisor { namespace testing { using %s = %s; } }" % (new_name, old_name))
        else:
            L.append("// gtest suite %s has a different fixture type in another file." % old_name)
        L.append("#define %s %s" % (old_name, new_name))
    if info["flags"]:
        f = info["flags"]
        L += ["// absl flags also defined (identically) by an earlier source become declarations.",
              '#include "absl/flags/declare.h"',
              '#include "absl/flags/flag.h"',
              "#undef ABSL_FLAG",
              "#define ABSL_FLAG(Type, name, default_value, help) GVT_FLAG_##name(Type, name, default_value, help)"]
        for n in f["declare"]:
            L.append("#define GVT_FLAG_%s(T, n, d, h) ABSL_DECLARE_FLAG(T, n)" % n)
        for n in f["define"]:
            L.append("#define GVT_FLAG_%s(T, n, d, h) ABSL_FLAG_IMPL(T, n, d, h)" % n)
    return "\n".join(L) + "\n"


def emit_manifest(ir):
    """C++ table for the dispatcher: one row per test target."""
    L = ["// GENERATED by gen/gen_build.py. Do not edit.",
         "// {name, renamed main or nullptr, {all test files}, {own srcs}, {instantiation prefixes}, {{renamed suite, upstream suite}}}"]
    tests = sorted(ir["tests"].values(), key=lambda b: b["name"])
    for b in tests:
        main = "gvt_main_%s" % ident(b["name"]) if b["main_srcs"] else "nullptr"
        files = ", ".join('"%s"' % f for f in b["test_files"])
        own = ", ".join('"%s"' % f for f in b["srcs"])
        pref = ", ".join('"%s"' % p for p in b["inst_prefixes"])
        ren = ", ".join('{"%s", "%s"}' % (n, o) for o, n in sorted(b["suite_renames"].items()))
        L.append('GVT_TARGET("%s", %s, GVT_LIST(%s), GVT_LIST(%s), GVT_LIST(%s), GVT_LIST(%s))'
                 % (b["name"], main, files, own, pref, ren))
    L.append("")
    L.append("// main() symbols renamed by objcopy; declared here so the dispatcher can call them.")
    for b in tests:
        if b["main_srcs"]:
            L.append("GVT_MAIN_DECL(gvt_main_%s)" % ident(b["name"]))
    return "\n".join(L) + "\n"


# IR (CMake-style) external name -> fbsource//third-party/abseil-cpp target.
# Verified against fbsource third-party/abseil-cpp/BUCK (2026-11-04). The OSS
# shim (shim/third-party/abseil-cpp/BUCK) uses the same target names, so one
# label serves both builds.
FB_ABSL = {
    "absl::algorithm_container": "algorithm__container",
    "absl::btree": "container__btree",
    "absl::cleanup": "cleanup",
    "absl::core_headers": "base__core_headers",
    "absl::flags": "flags__flag",
    "absl::flags_parse": "flags__parse",
    "absl::flat_hash_map": "container__flat_hash_map",
    "absl::flat_hash_set": "container__flat_hash_set",
    "absl::log": "log",
    "absl::memory": "memory",
    "absl::no_destructor": "base__no_destructor",
    "absl::node_hash_map": "container__node_hash_map",
    "absl::node_hash_set": "container__node_hash_set",
    "absl::optional": "types__optional",
    "absl::random_random": "random",
    "absl::span": "types__span",
    "absl::status": "status",
    "absl::statusor": "status__statusor",
    "absl::str_format": "strings__str_format",
    "absl::string_view": "strings__string_view",
    "absl::strings": "strings",
    "absl::synchronization": "synchronization",
    "absl::time": "time",
}


def buck_external(e):
    if e == "GTest::gtest":
        return "fbsource//third-party/googletest:gtest"
    if e == "GTest::gmock":
        return "fbsource//third-party/googletest:gmock"
    if e == "benchmark::benchmark":
        return "fbsource//third-party/benchmark:benchmark"
    if e in FB_ABSL:
        return "fbsource//third-party/abseil-cpp:" + FB_ABSL[e]
    raise SystemExit("emit_buck: unmapped external dependency %r; add it to "
                     "FB_ABSL (and the shim BUCK) deliberately" % e)


def buck_src(s):
    return "build/support/" + s[len("support/"):] if s.startswith("support/") else "upstream/" + s


def emit_buck(ir, mode):
    """Buck rules for the vendored corpus. mode: pertest | mega | both.

    Layout mirrors emit_cmake: one library per upstream cc_library, one
    whole-linked library per test source (gtest registration lives in
    static initializers), helpers as runfiles binaries. Vendored code is
    compiled as C++17 (upstream's own flags), which needs no local
    patches. The mega binary renames own-main symbols with -Dmain=
    instead of the prototype's objcopy step.
    """
    if mode not in ("pertest", "mega", "both"):
        raise SystemExit("emit_buck: bad mode %r" % mode)
    libs = ir["libraries"]
    tests = ir["tests"]
    allocator_label = "//test/util:errno_safe_allocator"
    allocator_src = "upstream/test/util/errno_safe_allocator.cc"

    used = {TEST_MAIN}
    for b in tests.values():
        used.update(b["lib_closure"])
    for h in ir["helpers"].values():
        if h.get("kind") == "cc_binary":
            used.update(h["deps"])
            if h.get("malloc"):
                used.add(h["malloc"])
    stack = list(used)
    while stack:
        for d in libs[stack.pop()]["deps"]:
            if d not in used:
                used.add(d)
                stack.append(d)
    used.discard(allocator_label)  # linked as a direct src, like D128212278

    def ext_deps(exts):
        return sorted({buck_external(e) for e in exts})

    def fmt_list(items, indent="        "):
        return ("\n" + indent).join('"%s",' % i for i in items)

    L = []
    w = L.append
    w("# Copyright (c) Meta Platforms, Inc. and affiliates.")
    w("# All rights reserved.")
    w("#")
    w("# This build configuration is licensed under the BSD-style license found in the")
    w("# LICENSE file in the root directory of this source tree.")
    w("#")
    w("# GENERATED by build/gen_build.py --emit buck --binaries %s. Do not edit." % mode)
    w("# Regenerate with scripts/refresh-gvisor-tests.py (see README.md).")
    w("")
    w('load("@fbsource//tools/build_defs:fb_native_wrapper.bzl", "fb_native")')
    w("")
    w('oncall("hermit")')
    w("")
    w("# Upstream headers at their upstream include paths (test/util/...).")
    w("fb_native.cxx_library(")
    w('    name = "gvisor_headers",')
    w('    exported_headers = {p[len("upstream/"):]: p for p in glob(["upstream/test/**/*.h"])},')
    w('    header_namespace = "",')
    w('    visibility = ["PUBLIC"],')
    w(")")
    w("")
    w("# Generated/support headers the combined binary force-includes.")
    w("fb_native.cxx_library(")
    w('    name = "gvisor_combined_headers",')
    w("    exported_headers = {")
    w('        "combined_manifest.inc": "build/combined_manifest.inc",')
    w('        "support/gvt_env_gate.h": "build/support/gvt_env_gate.h",')
    for s in sorted(ir["combined_only"]):
        w('        "combined_only/%s.h": "build/combined_only/%s.h",' % (ident(s), ident(s)))
    w("    },")
    w('    header_namespace = "",')
    w('    visibility = ["PUBLIC"],')
    w(")")
    w("")
    w("_COMMON_FLAGS = [\"-std=c++17\", \"-fno-omit-frame-pointer\"]")
    w("")

    w("# ---- libraries (%d used of %d parsed)" % (len(used), len(libs)))
    for label in sorted(used):
        l = libs[label]
        w("fb_native.cxx_library(")
        w('    name = "%s",' % lib_target(label))
        if l["srcs"]:
            w("    srcs = [\n        %s\n    ]," % fmt_list([buck_src(s) for s in l["srcs"]]))
        w("    compiler_flags = _COMMON_FLAGS,")
        w('    preferred_linkage = "static",')
        if l["defines"]:
            w("    preprocessor_flags = [%s]," % ", ".join('"-D%s"' % d for d in l["defines"]))
        if l["srcs"]:
            # Whole-linked: upstream library deps are cyclic
            # (temp_path <-> test_util <-> fs_util), so archive member
            # extraction order cannot resolve them; include every object.
            w("    link_whole = True,")
        # Bazel cc_library deps are public: consumers compile against these
        # headers (test_util.h includes gmock), so export them.
        deps = [":gvisor_headers"]
        deps += [":" + lib_target(d) for d in l["deps"] if d != allocator_label]
        deps += ext_deps(l["external"])
        w("    exported_deps = [\n        %s\n    ]," % fmt_list(deps))
        w('    visibility = ["PUBLIC"],')
        w(")")
        w("")

    all_srcs = {}
    for b in tests.values():
        for s in b["srcs"]:
            all_srcs.setdefault(s, []).append(b)

    def src_lib(s, suffix, extra_flags, extra_deps):
        users = all_srcs[s]
        deps = {":gvisor_headers"} | set(extra_deps)
        defines, copts = set(), set(extra_flags)
        for b in users:
            deps.update(":" + lib_target(d) for d in b["lib_closure"] if d != allocator_label)
            deps.update(ext_deps(b["external"]))
            defines.update("-D" + d for d in b["defines"])
            copts.update(b["copts"])
        w("fb_native.cxx_library(")
        w('    name = "%s%s",' % (src_target(s), suffix))
        w('    srcs = ["%s"],' % buck_src(s))
        w("    compiler_flags = _COMMON_FLAGS + [%s]," % ", ".join('"%s"' % c for c in sorted(copts)))
        if defines:
            w("    preprocessor_flags = [%s]," % ", ".join('"%s"' % d for d in sorted(defines)))
        w("    link_whole = True,")
        w('    preferred_linkage = "static",')
        w("    deps = [\n        %s\n    ]," % fmt_list(sorted(deps)))
        w('    visibility = ["PUBLIC"],')
        w(")")
        w("")

    w("# ---- per-source libraries (shared by per-test and combined binaries)")
    for s in sorted(all_srcs):
        src_lib(s, "", [], [])
    if mode in ("mega", "both"):
        w("# ---- combined-only recompiles (force-included generated headers)")
        for s in sorted(ir["combined_only"]):
            src_lib(s, "_c", ["-include", "combined_only/%s.h" % ident(s)],
                    [":gvisor_combined_headers"])
        w("# ---- own-main renames for the combined binary (-Dmain=...)")
        for s in sorted(all_srcs):
            owner = all_srcs[s][0]
            if s in owner["main_srcs"] and s not in ir["combined_only"]:
                src_lib(s, "_m", ["-Dmain=gvt_main_" + ident(owner["name"])], [])
            elif s in owner["main_srcs"]:
                src_lib(s, "_cm", ["-Dmain=gvt_main_" + ident(owner["name"]),
                                    "-include", "combined_only/%s.h" % ident(s)],
                        [":gvisor_combined_headers"])

    w("# ---- helper binaries (runfiles; stage beside the tests, see run-case)")
    for label in sorted(ir["helpers"]):
        h = ir["helpers"][label]
        if h["kind"] == "cc_binary":
            w("fb_native.cxx_binary(")
            w('    name = "%s",' % ("gvh_" + h["name"]))
            w("    srcs = [\n        %s\n    ]," % fmt_list([buck_src(s) for s in h["srcs"]]))
            w("    compiler_flags = _COMMON_FLAGS,")
            deps = [":gvisor_headers"] + [":" + lib_target(d) for d in h["deps"]] + ext_deps(h["external"])
            w("    deps = [\n        %s\n    ]," % fmt_list(deps))
            w('    visibility = ["PUBLIC"],')
            w(")")
            w("")
        else:
            w("# helper %s (%s): staged by the runner from upstream sources, not a Buck target"
              % (h["name"], h["kind"]))
            w("")

    if mode in ("pertest", "both"):
        w("# ---- per-test binaries (%d)" % len(tests))
        for label in sorted(tests):
            b = tests[label]
            w("fb_native.cxx_binary(")
            w('    name = "%s",' % b["name"])
            bin_srcs = [allocator_src] if b["malloc"] else []
            w("    srcs = [%s]," % ", ".join('"%s"' % s for s in bin_srcs))
            w("    compiler_flags = _COMMON_FLAGS,")
            if b["linkopts"]:
                w("    linker_flags = [%s]," % ", ".join('"%s"' % o for o in b["linkopts"]))
            deps = [":" + src_target(s) for s in b["srcs"]]
            if b["uses_test_main"] and not b["main_srcs"]:
                deps.append(":" + lib_target(TEST_MAIN))
            # Full closure, dependents first: static-archive link order.
            deps += [":" + lib_target(d) for d in b["lib_closure"] if d != allocator_label]
            deps += ext_deps(b["external"])
            w("    deps = [\n        %s\n    ]," % fmt_list(deps))
            w('    visibility = ["PUBLIC"],')
            w(")")
            w("")

    if mode in ("mega", "both"):
        w("# ---- combined binary (one process runs one target's cases)")
        w("fb_native.cxx_binary(")
        w('    name = "gvisor_tests",')
        w('    srcs = ["build/support/combined_main.cc", "%s"],' % allocator_src)
        w('    compiler_flags = _COMMON_FLAGS + ["-DGVT_MAIN_CPP_LINKAGE"],')
        w('    linker_flags = ["-Wl,--wrap=_ZN6gvisor7testing11RunAllTestsEv"],')
        deps = {":gvisor_combined_headers"}
        for s in sorted(all_srcs):
            owner = all_srcs[s][0]
            if s in owner["main_srcs"]:
                deps.add(":" + src_target(s) + ("_cm" if s in ir["combined_only"] else "_m"))
            elif s in ir["combined_only"]:
                deps.add(":" + src_target(s) + "_c")
            else:
                deps.add(":" + src_target(s))
        for b in tests.values():
            deps.update(":" + lib_target(d) for d in b["lib_closure"]
                        if d not in (allocator_label, TEST_MAIN))
            deps.update(ext_deps(b["external"]))
        w("    deps = [\n        %s\n    ]," % fmt_list(sorted(deps)))
        w('    visibility = ["PUBLIC"],')
        w(")")
        w("")
    return "\n".join(L) + "\n"


def read_allowlist(path):
    names = []
    for line in open(path):
        line = line.split("#")[0].strip()
        if line:
            names.append(line)
    return None if names == ["*"] else names


def all_outputs(ir, mode):
    return {
        "targets.json": json.dumps(ir, indent=1, sort_keys=True) + "\n",
        **{"combined_only/%s.h" % ident(src): emit_combined_only(src, info)
           for src, info in ir["combined_only"].items()},
        "gvisor_tests.cmake": emit_cmake(ir),
        "combined_manifest.inc": emit_manifest(ir),
        "../BUCK": emit_buck(ir, mode),
    }


def main():
    ap = argparse.ArgumentParser()
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--gvisor", default=os.path.join(root, "upstream"))
    ap.add_argument("--out", default=here)
    ap.add_argument("--allowlist", default=os.path.join(here, "allowlist.txt"),
                    help="file of vendored test target names; empty string means all")
    ap.add_argument("--binaries", default="both", choices=["pertest", "mega", "both"],
                    help="binary shape emitted into BUCK (switching is this flag, not a rewrite)")
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    allowlist = read_allowlist(a.allowlist) if a.allowlist else None
    ir = build_ir(a.gvisor, allowlist)
    outputs = all_outputs(ir, a.binaries)
    if a.check:
        bad = []
        for name, text in outputs.items():
            p = os.path.join(a.out, name)
            if not os.path.exists(p) or open(p).read() != text:
                bad.append(name)
        if bad:
            print("generated files are stale: %s" % ", ".join(bad))
            return 1
        print("generated files are up to date")
        return 0
    os.makedirs(os.path.join(a.out, "combined_only"), exist_ok=True)
    for stale in os.listdir(os.path.join(a.out, "combined_only")):
        if "combined_only/" + stale not in outputs:
            os.remove(os.path.join(a.out, "combined_only", stale))
    for name, text in outputs.items():
        with open(os.path.join(a.out, name), "w") as fh:
            fh.write(text)
    t = ir["tests"]
    print("absl flag names defined in >1 source: %d (%s); sources recompiled for the combined binary: %d"
          % (len(ir["flag_collisions"]), ", ".join(sorted(ir["flag_collisions"])), len(ir["flag_share"])))
    print("gtest suite names with >1 fixture type: %d (%s); suite renames: %d"
          % (len(ir["suite_collisions"]), ", ".join(sorted(ir["suite_collisions"])),
             sum(len(v["suite_renames"]) for v in ir["combined_only"].values())))
    print("parameterized instantiations sharing a prefix across files: %d (%s)"
          % (len(ir["instantiation_collisions"]), ", ".join(sorted(ir["instantiation_collisions"]))))
    print("files registering a gtest global environment (gated): %d (%s)"
          % (len(ir["global_environment_files"]), ", ".join(ir["global_environment_files"])))
    print("tests: %d (own main: %d), helpers: %d, libraries parsed: %d, shared test srcs: %d"
          % (len(t), sum(1 for b in t.values() if b["main_srcs"]), len(ir["helpers"]),
             len(ir["libraries"]), len(ir["shared_test_srcs"])))
    for w in ir["warnings"]:
        print("warning:", w)
    return 0


if __name__ == "__main__":
    sys.exit(main())
