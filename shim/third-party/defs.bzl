# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under both the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree and the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree.


HOMEBREW_CONSTRAINT = "//os:macos-homebrew"

def system_library(name: str, packages = None, visibility = ["PUBLIC"], deps = [], exported_deps = [], **kwargs):
    system_packages_target_name = "__{}_system_pkgs".format(name)
    packages = packages or dict()
    packages["DEFAULT"] = []
    system_packages(
        name = system_packages_target_name,
        packages = select(packages),
    )
    deps = deps + [":" + system_packages_target_name]

    brews = packages.get(HOMEBREW_CONSTRAINT)
    if brews != None:
        exported_deps = exported_deps + select({
            HOMEBREW_CONSTRAINT: _system_homebrew_targets(name, brews),
            "DEFAULT": [],
        })

    native.prebuilt_cxx_library(name = name, visibility = visibility, deps = deps, exported_deps = exported_deps, **kwargs)

def external_pkgconfig_library(name, package = None, visibility = ["PUBLIC"], labels = [], default_target_platform = "prelude//platforms:default", deps = []):
    """prelude//third-party:pkgconfig.bzl's external_pkgconfig_library, except that the
    two `pkg-config` genrules are forced LOCAL. The prelude passes the buck1 attribute
    `remote = False`, which buck2 ignores, so under a remote execution platform the
    genrules ran on RE workers, which have no .pc files (libunwind-ptrace). pkg-config
    describes the host, so it must run on the host; the "non_deterministic_build_info"
    label is the prelude's way to require that."""
    if package == None:
        package = name
    local = ["non_deterministic_build_info"]
    pkg_config_cflags = name + "__pkg_config_cflags"
    native.genrule(
        name = pkg_config_cflags,
        default_target_platform = default_target_platform,
        out = "out",
        cmd = "pkg-config --cflags {} > $OUT".format(package),
        labels = local,
    )
    pkg_config_libs = name + "__pkg_config_libs"
    native.genrule(
        name = pkg_config_libs,
        default_target_platform = default_target_platform,
        out = "out",
        cmd = "pkg-config --libs {} > $OUT".format(package),
        labels = local,
    )
    native.prebuilt_cxx_library(
        name = name,
        default_target_platform = default_target_platform,
        visibility = visibility,
        exported_preprocessor_flags = ["@$(location :{})".format(pkg_config_cflags)],
        exported_linker_flags = ["@$(location :{})".format(pkg_config_libs)],
        exported_deps = deps,
        labels = list(labels) + ["third-party:pkg-config:{}".format(package)],
    )

def pkgconfig_system_library(name: str, pkgconfig_name = None, packages = None, visibility = ["PUBLIC"], deps = [], exported_deps = [], unsupported = dict(), **kwargs):
    system_packages_target_name = "__{}_system_pkgs".format(name)
    packages = packages or dict()
    packages["DEFAULT"] = []
    system_packages(
        name = system_packages_target_name,
        packages = select(packages),
    )

    deps = exported_deps + deps

    if len(unsupported) == 0:
        external_pkgconfig_library(name = name, package = pkgconfig_name, visibility = visibility, deps = deps + [":" + system_packages_target_name], **kwargs)
    else:
        exported_deps_select_map = {}
        for constraint, constraint_exported_deps in unsupported.items():
            if constraint == HOMEBREW_CONSTRAINT:
                brews = packages.get(constraint, [])
                constraint_exported_deps = constraint_exported_deps + _system_homebrew_targets(name, brews)

            exported_deps_select_map[constraint] = constraint_exported_deps

        pkgconfig_target_name = "__{}_pkgconfig".format(name)
        external_pkgconfig_library(name = pkgconfig_target_name, package = pkgconfig_name, visibility = [], deps = deps, **kwargs)
        exported_deps_select_map["DEFAULT"] = [":" + pkgconfig_target_name]

        native.prebuilt_cxx_library(
            name = name,
            visibility = visibility,
            deps = [":" + system_packages_target_name],
            exported_deps = select(exported_deps_select_map),
        )

def _system_homebrew_targets(name: str, brews):
    deps = []
    for brew in brews:
        homebrew_target_name = "__{}_homebrew_{}".format(name, brew)
        homebrew_library(
            name = homebrew_target_name,
            brew = brew,
        )
        deps.append(":" + homebrew_target_name)

    return deps

def _system_packages_impl(ctx: AnalysisContext) -> list[Provider]:
    return [DefaultInfo()]

system_packages = rule(
    impl = lambda _ctx: [DefaultInfo()],
    attrs = {
        "deps": attrs.list(attrs.dep(), default = []),
        "packages": attrs.list(attrs.string()),
    },
)

def homebrew_library(
    name: str,
    brew: str,
    homebrew_header_path = "include",
    exported_preprocessor_flags = [],
    exported_linker_flags = [],
    target_compatible_with = ["//os:macos-homebrew"],
    **kwargs,
):
    preproc_flags_rule_name = "__{}__{}__preproc_flags".format(name, brew)
    native.genrule(
        name = preproc_flags_rule_name,
        type = "homebrew_library_preproc_flags",
        out = "out",
        cmd = 'echo "-I`brew --prefix {}`/{}" > $OUT'.format(brew, homebrew_header_path),
        target_compatible_with = target_compatible_with,
    )

    linker_flags_rule_name = "__{}__{}__linker_flags".format(name, brew)
    native.genrule(
        name = linker_flags_rule_name,
        type = "homebrew_library_linker_flags",
        out = "out",
        cmd = 'echo "-L`brew --prefix {}`/lib" > $OUT'.format(brew),
        target_compatible_with = target_compatible_with,
    )

    native.prebuilt_cxx_library(
        name = name,
        exported_preprocessor_flags = exported_preprocessor_flags
        + [
            "@$(location :{})/preproc_flags.txt".format(preproc_flags_rule_name),
        ],
        exported_linker_flags = exported_linker_flags
        + [
            "@$(location :{})/linker_flags.txt".format(linker_flags_rule_name),
        ],
        target_compatible_with = target_compatible_with,
        **kwargs,
    )

def host_system_libraries(libraries, packages):
    """System libraries, as pkg-config describes the host, except under the RE modes.

    libraries maps a target name to (pkg-config name, linker flags). Under the RE modes
    (shim/modes/hybrid and remote, which set hermit.rust_sysroot) links run on RE
    workers, which lack the -devel packages; there each target links the host's library
    files that shim/modes/stage-re-inputs copied into this package's staged/ directory,
    using the given linker flags, and the files travel with the link as an input.
    Otherwise each target is a pkgconfig_system_library, as before."""
    if read_config("hermit", "rust_sysroot", ""):
        native.export_file(
            name = "staged",
            src = "staged",
        )
        for name, (_pkgconfig_name, flags) in libraries.items():
            native.prebuilt_cxx_library(
                name = name,
                exported_linker_flags = ["-L$(location :staged)"] + flags,
                visibility = ["PUBLIC"],
            )
    else:
        for name, (pkgconfig_name, _flags) in libraries.items():
            pkgconfig_system_library(
                name = name,
                packages = packages,
                pkgconfig_name = pkgconfig_name,
            )
