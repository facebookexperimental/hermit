/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Link the common engine against the guest's GNU libc without a build-root
//! libgcc_s dependency or Nix library RUNPATH. The maintained per-DSO hidden
//! glibc compatibility definitions remain in src/lib.rs's common include.
//!
//! This preserves the existing LiteInst/SaBRe static unwinder recipe. The
//! installed engine still must pass the strict guest-preload ELF contract;
//! successful linking alone is not portability qualification.

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
