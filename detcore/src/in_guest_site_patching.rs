/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The check that an in-guest Tool image runs with syscall site patching off,
//! and collects the statistics it reports at exit, when the host asked for it
//! (`Config::in_guest_site_patching_off`, set for
//! `hermit --backend=in-guest-trap`).
//!
//! The in-guest LiteInst runtime takes both settings from the guest
//! environment while it installs the Tool: `REVERIE_LITEINST_SITE_PATCHING` and
//! the statistics coordinator's variable. Hermit sets them, but code that runs
//! in the guest before the runtime's constructor (a constructor of one of the
//! program's own shared libraries), or code the runtime calls through
//! interposable symbols during installation, could change them. So the check
//! does not read the environment: the Tool's constructor records the host's
//! request, which arrived in the configuration handshake that guest code
//! cannot change ([`require_site_patching_off`]), and once installation has
//! returned the runtime's preload constructor (`detcore_liteinst_initialize`)
//! compares it with the settings the runtime actually captured and refuses to
//! run the program when they differ ([`violation`]).

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

static REQUIRED: AtomicBool = AtomicBool::new(false);

/// Records that the host asked this Tool image to run with syscall site
/// patching off and statistics on. Called from the Tool's constructor.
pub fn require_site_patching_off() {
    REQUIRED.store(true, Ordering::Release);
}

/// Whether the host asked this Tool image to run with syscall site patching
/// off ([`require_site_patching_off`]).
pub fn site_patching_off_required() -> bool {
    REQUIRED.load(Ordering::Acquire)
}

/// Why an image whose runtime captured `runtime_patches` (whether it patches
/// syscall sites) and `runtime_collects` (whether it collects its exit
/// statistics) breaks the host's request, or `None` when it does not or when
/// nothing was requested (`required` false).
pub fn violation(required: bool, runtime_patches: bool, runtime_collects: bool) -> Option<String> {
    if !required {
        return None;
    }
    if runtime_patches {
        return Some(
            "--backend=in-guest-trap runs with syscall site patching off, but the in-guest \
             runtime installed this process with site patching on: it reads \
             REVERIE_LITEINST_SITE_PATCHING from the environment, and code in the guest that \
             ran before it (a shared library's constructor, or a function the runtime calls \
             while installing) changed the value Hermit set; run this program with \
             --backend=liteinst"
                .to_owned(),
        );
    }
    (!runtime_collects).then(|| {
        "--backend=in-guest-trap needs every process's syscall site statistics, but the \
         in-guest runtime installed this process without collecting them: it reads its \
         statistics socket from the environment, and code in the guest that ran before it \
         removed the variable Hermit set, so this process could not show that it patched \
         nothing; run this program with --backend=liteinst"
            .to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_captured_settings_decide_and_only_when_requested() {
        assert_eq!(violation(true, false, true), None);
        let patches = violation(true, true, true).expect("a patching runtime is refused");
        assert!(
            patches.contains("installed this process with site patching on"),
            "{patches}"
        );
        assert!(patches.contains("--backend=liteinst"), "{patches}");
        // Patching is the first reason even when statistics are missing too.
        assert_eq!(violation(true, true, false), Some(patches));
        let silent = violation(true, false, false).expect("an image that reports nothing");
        assert!(silent.contains("without collecting them"), "{silent}");
        for (patches, collects) in [(true, true), (true, false), (false, false)] {
            assert_eq!(violation(false, patches, collects), None);
        }
    }
}
