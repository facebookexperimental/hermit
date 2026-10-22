/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keeps libdetcore_liteinst.so loadable by guests whose glibc is not the
//! build root's (https://github.com/rrnewton/hermit/issues/3967), as
//! detcore-sabre/build.rs does for the SaBRe plugin
//! (https://github.com/rrnewton/hermit/issues/3652).
//!
//! Each guest's own dynamic loader preloads this runtime, against the libc
//! that guest already has, so the runtime may depend only on libraries every
//! glibc provides and may search no build-root directory for them:
//!
//! - The unwinder is linked from libgcc_eh.a instead of libgcc_s.so.1. A
//!   host guest has no libgcc_s loaded, and the build root's copy needs a newer
//!   glibc than the host's. `-u _Unwind_RaiseException` makes the linker
//!   extract the unwinder when it reaches libgcc_eh.a, which precedes the
//!   standard library's `-lgcc_s`; `--as-needed` then drops libgcc_s.so.1.
//!   The cdylib's version script keeps the unwinder's symbols local.
//! - `NIX_DONT_SET_RPATH_<target>` stops the Nix linker wrapper from
//!   recording its glibc and gcc library directories as the runtime's RUNPATH.
//!   With that RUNPATH a host guest, which has not loaded libm.so.6 when the
//!   runtime asks for it (the runtime calls exp, log and pow), gets the build
//!   root's libm, which needs the build root's libc; the guest's loader stops
//!   with "version `GLIBC_ABI_DT_RELR' not found" and the guest exits before
//!   it connects to the coordinator. Without a RUNPATH, libm.so.6 resolves
//!   wherever the guest's own loader finds its libc. The wrapper reads only
//!   the name suffixed with the target triple, `-` spelled `_`; other linkers
//!   ignore it.
//!
//! src/lib.rs compiles detcore-sabre/src/glibc_compat.rs for the symbols that
//! statically linked code would otherwise import at a newer version, and
//! ci/verify-hermit-e2e-artifact.sh refuses an artifact whose runtime needs
//! more than glibc 2.34.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "linux" || target_env != "gnu" {
        return;
    }
    // `-bundle`: the archive is found by the C compiler driver at link time,
    // not by rustc while it writes the rlib, as in the standard library's own
    // `unwind` crate.
    println!("cargo:rustc-link-lib=static:-bundle=gcc_eh");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-u,_Unwind_RaiseException");
    let target = env::var("TARGET").unwrap_or_default().replace('-', "_");
    println!("cargo:rustc-env=NIX_DONT_SET_RPATH_{target}=1");
}
