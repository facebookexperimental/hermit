# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

load("@fbsource//tools/build_defs:rust_binary.bzl", "rust_binary")
load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")

def hermit_release_targets(srcs, deps, unstamped_env):
    """:_libhermit-release and :_hermit-release, libhermit and hermit as the
    release build compiles them, from libhermit's `srcs` and `deps`.

    The open-source build's release entrypoint, scripts/build-buck-release.rs,
    stamps a release through Buck config values (never shell-expanded source
    text), and a stamped build adds the buck-release-provenance cfg, whose
    compile-time checks refuse an empty identity. Regular open-source builds
    and CI (ci/buck-e2e, `validate --build buck-*`) pass
    -c hermit_release.unstamped=true: the release feature set and linkage
    with the ordinary build identity, `unstamped_env`, so a commit that
    changes no compiled source stays cached. fbsource, whose root config has
    no [oss] section, always builds these targets unstamped; it packages
    Hermit through fb-only/BUCK.
    """
    open_source = read_root_config("oss", "internal_cell", "") != ""
    release_unstamped = not open_source or read_root_config("hermit_release", "unstamped", "false") == "true"

    # Each case spells out its whole list because
    # scripts/check-buck-reindeer-features.rs reads only literal `features`
    # lists, never a variable or a concatenation.
    release_features = (
        {
            "features": [
                "liteinst",
            ]
        }
        if release_unstamped
        else {
            "features": [
                "buck-release-provenance",
                "liteinst",
            ]
        }
    )
    release_build_env = (
        unstamped_env
        if release_unstamped
        else {
            "CARGO_PKG_VERSION": read_root_config("hermit_release", "version", ""),
            "HERMIT_BUILD_DATE": read_root_config("hermit_release", "build_date", ""),
            "HERMIT_BUILD_GIT_SHA": read_root_config("hermit_release", "hermit_sha", ""),
            "HERMIT_REVERIE_PIN": read_root_config("hermit_release", "reverie_sha", ""),
        }
    )

    # The release binary's link inputs. The open-source release entrypoint
    # names content-bound prebuilt packages here; the defaults are the
    # declared third-party targets.
    release_lzma_target = read_root_config(
        "hermit_release",
        "lzma_target",
        "fbsource//third-party/xz:lzma",
    )
    release_unwind_target = read_root_config(
        "hermit_release",
        "unwind_target",
        "fbsource//third-party/libunwind:unwind-ptrace",
    )

    rust_library(
        name = "_libhermit-release",
        srcs = srcs,
        autocargo = {"ignore_rule": True},
        crate = "hermit",
        env = release_build_env,
        named_deps = {"reverie": "//hermetic_infra/reverie:reverie"},
        # As :libhermit: its link contracts need a final binary link.
        preferred_linkage = "static",
        # hermit/detcore virtualizes the x86 cpuid instruction; x86-64 only.
        target_compatible_with = ["ovr_config//cpu:x86_64"],
        unittests = False,
        deps = deps,
        **release_features,
    )

    # The release binary's runtime library search path, as a DT_RPATH beside
    # the published layout. In the open-source build rustc hands -Clink-arg to
    # the linker; fbsource links Rust through the C++ toolchain, which refuses
    # -Clink-arg and takes the target's `linker_flags`.
    rpath_flags = [
        "-Wl,--disable-new-dtags",
        "-Wl,-rpath,$ORIGIN/../install_pkg/rsrcs/hermit-runtime:$ORIGIN/install/rsrcs/hermit-runtime",
    ]
    if open_source:
        rpath = {"rustc_flags": [arg for flag in rpath_flags for arg in ("-C", "link-arg=" + flag)]}
    else:
        rpath = {"linker_flags": rpath_flags}

    rust_binary(
        name = "_hermit-release",
        srcs = ["src/bin/hermit/main.rs"],
        # The system allocator, as :hermit uses.
        allocator = "malloc",
        autocargo = {"ignore_rule": True},
        crate = "hermit",
        crate_root = "src/bin/hermit/main.rs",
        env = release_build_env,
        link_group_map = [],
        link_style = "static_pic",
        # hermit/detcore virtualizes the x86 cpuid instruction; x86-64 only.
        target_compatible_with = ["ovr_config//cpu:x86_64"],
        unittests = False,
        deps = [
            release_lzma_target,
            release_unwind_target,
            ":_libhermit-release",
            "//common/rust/shed/fbinit:fbinit",
        ],
        **(release_features | rpath),
    )

def hermit_release_aliases():
    # hermit-release / libhermit-release pin the release binaries to the
    # OSS-only shim platform through configured_alias, a raw Buck rule that
    # fbcode build files may not call. Define the aliases only in the
    # GitHub build, whose root config carries [oss] (internal_cell);
    # fbcode's root config has no such section, so these targets simply do
    # not exist internally, as before the release pipeline.
    if read_root_config("oss", "internal_cell", "") == "":
        return

    native.configured_alias(
        name = "libhermit-release",
        actual = ":_libhermit-release",
        platform = "shim//:hermit_rust_release_platform",
        visibility = ["PUBLIC"],
    )

    native.configured_alias(
        name = "hermit-release",
        actual = ":_hermit-release",
        platform = "shim//:hermit_rust_release_platform",
        visibility = ["PUBLIC"],
    )
