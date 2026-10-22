/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Named readings of one bundled backend capability.
//!
//! `BackendCapabilities::supports_blocked_wait_signal_interruption` says that
//! blocked waits decide signal interruption from the kernel's signal state
//! (Detcore's B1 mechanism). Many readers that are not about blocked waits
//! read it too, because every backend that sets it (ptrace, and e9patch, which
//! runs under ptrace) also has the facts those readers need. Each function
//! here names one of those facts for its readers.
//!
//! These are **legacy bundled conditions**, not separate capabilities: each is
//! exactly today's expression over the one capability, so every backend keeps
//! its behaviour. Some name a fact that is also true elsewhere. In-guest
//! LiteInst and SaBRe run guest threads as host tasks too
//! (`procfs_inode::guest_tids_name_host_tasks` in Detcore is true for them),
//! yet [`host_task_reads_enabled`] is false there. Widening any of these to its
//! truthful per-backend value changes that backend's behaviour and needs its
//! own review; it is deferred work in step 3.3 of the A5 design
//! (<https://github.com/rrnewton/hermit/issues/4485>, dev-hermit
//! `ai_docs/2026-10-16-a5-signal-delivery-design.md` section 15.12).

use reverie::BackendCapabilities;

use crate::config::Config;

/// Whether Detcore reads or signals a guest task through the host task of the
/// same ID: the host `/proc/<pid>/maps` for a futex address, a timespec read
/// that falls back to host memory maps, `/proc/<tid>` for what a descriptor
/// names, the `rt_sigqueueinfo` precheck, and the parent-death signal it sends
/// itself.
///
/// Legacy bundled condition: true exactly where
/// `supports_blocked_wait_signal_interruption` is (ptrace, e9patch).
pub fn host_task_reads_enabled(backend: &BackendCapabilities) -> bool {
    backend.supports_blocked_wait_signal_interruption
}

/// Whether the kernel's signal state for a guest thread is the guest's own:
/// the thread is a host task, and no runtime in the guest's process installs
/// handlers or masks of its own. Detcore's signal-state copy is kept, and
/// compared with the kernel, only where this holds, and only here does
/// Detcore decide a process-directed signal to its own process from the
/// kernel's per-thread signal sets.
///
/// Legacy bundled condition: true exactly where
/// `supports_blocked_wait_signal_interruption` is (ptrace, e9patch).
pub fn kernel_signal_state_is_guests(backend: &BackendCapabilities) -> bool {
    backend.supports_blocked_wait_signal_interruption
}

/// Whether Detcore treats signals that Linux sends at host-chosen moments as
/// reaching the guest from the host: a terminal's signals, a child's exit
/// signal, the signals an orphaned process group receives, and those an
/// `fcntl` (async I/O, leases, directory notification), `ioctl`, `open` or
/// `prctl` arms. Where this holds, Detcore records them as host-timed so that
/// no scheduler decision depends on when they arrive. Such signals may reach
/// the guest on other backends too; false is not a statement that they do
/// not.
///
/// Legacy bundled condition: true exactly where
/// `supports_blocked_wait_signal_interruption` is (ptrace, e9patch).
pub fn host_timed_signal_sources_are_physical(backend: &BackendCapabilities) -> bool {
    backend.supports_blocked_wait_signal_interruption
}

/// Whether the scheduler models which thread the kernel gives a signal, from
/// the signal masks it reads from the kernel, and keeps the per-process ledger
/// of signals discarded at send time; the guest side publishes a waiting
/// thread's mask and records handler-restored real-time signals for it
/// (`publish_wait_mask`, `record_handler_restore_mask`). It needs serialized
/// threads, so that a mask it reads cannot change before the kernel acts on
/// it.
///
/// Legacy bundled condition: true exactly where threads are serialized and
/// `supports_blocked_wait_signal_interruption` holds (ptrace, e9patch).
pub fn models_signal_targets(cfg: &Config) -> bool {
    cfg.sequentialize_threads && cfg.backend.supports_blocked_wait_signal_interruption
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named reading equals the legacy expression its readers used
    /// before (the bundled capability, and for `models_signal_targets` also
    /// serialized threads), on every backend and under both thread modes: no
    /// backend's behaviour moves.
    #[test]
    fn every_named_reading_is_its_legacy_expression_on_every_backend() {
        // Only ptrace and e9patch set the bundled capability today.
        let backends = [
            ("ptrace", BackendCapabilities::PTRACE, true),
            ("e9patch", BackendCapabilities::E9PATCH, true),
            ("liteinst", BackendCapabilities::LITEINST_IN_GUEST, false),
            ("sabre", BackendCapabilities::SABRE, false),
            ("dbt", BackendCapabilities::DBT, false),
            ("kvm", BackendCapabilities::KVM, false),
            (
                "unprepared",
                crate::config::DEFAULT_BACKEND_CAPABILITIES,
                false,
            ),
        ];
        for (name, backend, expected) in backends {
            let bundled = backend.supports_blocked_wait_signal_interruption;
            assert_eq!(bundled, expected, "{name}");
            assert_eq!(host_task_reads_enabled(&backend), bundled, "{name}");
            assert_eq!(kernel_signal_state_is_guests(&backend), bundled, "{name}");
            assert_eq!(
                host_timed_signal_sources_are_physical(&backend),
                bundled,
                "{name}"
            );
            for sequentialize_threads in [false, true] {
                let cfg = Config {
                    sequentialize_threads,
                    backend,
                    ..Config::default()
                };
                assert_eq!(
                    models_signal_targets(&cfg),
                    sequentialize_threads && bundled,
                    "{name}, sequentialize_threads={sequentialize_threads}"
                );
            }
        }
    }
}
