load("@fbcode_macros//build_defs:export_files.bzl", "export_file")
load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")

oncall("hermit")

# Request :hermit-inguest-engine[cdylib] explicitly. The default is a check
# output; [shared]/[dylib] is not this engine's C ABI artifact.
rust_library(
    name = "hermit-inguest-engine",
    srcs = [
        "inguest-engine/build.rs",
        "inguest-engine/src/install.rs",
        "inguest-engine/src/lib.rs",
        "inguest-engine/src/loader_settings.rs",
        "inguest-engine/src/loader_state.rs",
        "inguest-engine/src/native/abi.rs",
        "inguest-engine/src/native/auxv.rs",
        "inguest-engine/src/native/bootstrap.rs",
        "inguest-engine/src/native/config.rs",
        "inguest-engine/src/native/copied.rs",
        "inguest-engine/src/native/copied_action.rs",
        "inguest-engine/src/native/copied_policy.rs",
        "inguest-engine/src/native/diagnostics.rs",
        "inguest-engine/src/native/log_record.rs",
        "inguest-engine/src/native/entry.rs",
        "inguest-engine/src/native/first_instruction_observer.rs",
        "inguest-engine/src/native/image.rs",
        "inguest-engine/src/native/lifecycle.rs",
        "inguest-engine/src/native/mod.rs",
        "inguest-engine/src/native/owner.rs",
        "inguest-engine/src/native/policy.rs",
        "inguest-engine/src/native/prelude.rs",
        "inguest-engine/src/observation.rs",
        "inguest-engine/src/runtime.rs",
        "common/inguest/glibc_compat.rs",
    ],
    crate = "hermit_inguest_engine",
    crate_root = "inguest-engine/src/lib.rs",
    edition = "2024",
    preferred_linkage = "any",
    # This engine supports GNU Linux x86-64 only. Keep the Cargo build.rs
    # static unwinder / no build-root RUNPATH policy in the native output.
    rustc_flags = ["-lstatic:-bundle=gcc_eh"],
    linker_flags = ["-Wl,-u,_Unwind_RaiseException"],
    env = {"NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu": "1"},
    features = [],
    unittests = False,
    doctests = False,
    autocargo = {
        "cargo_toml_dir": "inguest-engine",
        "cargo_toml_config": {
            "package": {
                "name": "hermit-inguest-engine",
                "version": "0.5.4",
                "build": "build.rs",
                "publish": False,
            },
            "lib": {
                "name": "hermit_inguest_engine",
                "crate-type": ["cdylib"],
                "test": False,
                "doctest": False,
                "bench": False,
            },
            "features": {
                "default": [],
                "dispatch-observer": ["detcore/dispatch-observer"],
            },
            "dependencies_override": {
                "dependencies": {
                    "reverie-inguest": {
                        "default-features": False,
                        "features": ["coordinator-rpc", "rcb-clock"],
                    },
                    "reverie-liteinst": {"default-features": False},
                },
            },
        },
    },
    deps = [
        "fbsource//third-party/rust:libc",
        "fbsource//third-party/rust:rand",
        "fbsource//third-party/rust:tracing",
        "//hermetic_infra/hermit/detcore:detcore",
        "//hermetic_infra/reverie:reverie",
        "//hermetic_infra/reverie:reverie-inguest",
        "//hermetic_infra/reverie:reverie-liteinst",
        "//hermetic_infra/reverie:reverie-rpc-transport",
    ],
)

# Export the maintained header, never a second generated/copy-owned header.
export_file(
    name = "hermit-engine-v1-header",
    src = "inguest-engine/include/hermit-engine-v1.h",
)

# Native callers consume the source-owned scalar header from the same cohort.
export_file(
    name = "hermit-engine-native-v1-header",
    src = "inguest-engine/include/hermit-engine-native-v1.h",
)
