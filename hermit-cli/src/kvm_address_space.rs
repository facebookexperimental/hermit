/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Refuse to start the KVM backend under a finite address-space limit.
//!
//! reverie-kvm reserves host address space for guest memory (its README,
//! "Host address space"): the guest memory size Hermit gives it for every
//! live guest process, once more while an exec validates its new image, and,
//! for an exited child, until the child's host thread releases it, which can
//! be after the parent has gone on. `RLIMIT_AS` counts every reserved byte,
//! though only the pages a guest touches use memory. The peak therefore
//! follows the guest's process tree and the host's thread scheduling, and a
//! run under a finite limit stops wherever those put it: on the same command
//! one run completes and the next does not
//! (<https://github.com/rrnewton/reverie/issues/1225>). Hermit refuses such a
//! run before the guest starts instead, by name and on every run.
//!
//! This is launch policy, about Hermit's own process. It never raises the
//! limit, and it does not touch the guest's view of `RLIMIT_AS`, which
//! Detcore models as before. The backend still refuses, by name, an exec
//! whose memory a host refuses for another reason (a limit lowered after
//! launch, `vm.max_map_count`), and the run ends on that refusal.

use std::fmt;

/// A named, pre-guest refusal of a finite `RLIMIT_AS` on the KVM backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvmAddressSpaceRefusal {
    soft: libc::rlim_t,
    hard: libc::rlim_t,
    guest_memory_bytes: usize,
}

impl KvmAddressSpaceRefusal {
    /// The refusal for `limit`, the `RLIMIT_AS` Hermit would run the backend
    /// under, when the backend reserves `guest_memory_bytes` for each guest
    /// memory. `None` for an unlimited soft limit: the kernel enforces the
    /// soft limit, and only an unlimited one refuses no reservation however
    /// large the guest's process tree grows.
    pub fn for_limit(limit: libc::rlimit, guest_memory_bytes: usize) -> Option<Self> {
        (limit.rlim_cur != libc::RLIM_INFINITY).then_some(Self {
            soft: limit.rlim_cur,
            hard: limit.rlim_max,
            guest_memory_bytes,
        })
    }
}

/// A size in bytes as the refusal prints it: in GiB, with the exact byte
/// count, for example `3 GiB (3221225472 bytes)`.
fn size_text(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    if bytes.is_multiple_of(GIB) {
        format!("{} GiB ({bytes} bytes)", bytes / GIB)
    } else {
        format!("{:.2} GiB ({bytes} bytes)", bytes as f64 / GIB as f64)
    }
}

/// An `RLIMIT_AS` value as the refusal prints it.
fn limit_text(value: libc::rlim_t) -> String {
    if value == libc::RLIM_INFINITY {
        "unlimited".to_owned()
    } else {
        size_text(value)
    }
}

impl fmt::Display for KvmAddressSpaceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let guest_memory = size_text(self.guest_memory_bytes as u64);
        write!(
            f,
            "kvm_address_space_limit: --backend=kvm refuses to start under a finite RLIMIT_AS \
             (soft {}, hard {}): the KVM backend reserves {guest_memory} of host address space \
             for every guest process, plus {guest_memory} more for each exec in flight, and \
             RLIMIT_AS counts every reserved byte although only the pages a guest touches use \
             memory, so no finite limit fits every process tree, and under one a run would stop \
             wherever the guest's process tree and host thread scheduling put the peak; Hermit \
             does not raise the limit itself; run Hermit with an unlimited address-space limit \
             (`ulimit -v unlimited`, which needs an unlimited hard limit or privilege) and bound \
             its memory with a cgroup instead (memory.max, which charges the memory the run \
             uses, not the address space it reserves); the guest has not started",
            limit_text(self.soft),
            limit_text(self.hard),
        )
    }
}

impl std::error::Error for KvmAddressSpaceRefusal {}

/// Refuses ([`KvmAddressSpaceRefusal`]) when this process, which is about to
/// run the KVM backend, has a finite soft `RLIMIT_AS`; the backend would
/// reserve `guest_memory_bytes` for each guest memory.
pub fn admit_kvm_address_space(guest_memory_bytes: usize) -> anyhow::Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only `limit`.
    if unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut limit) } != 0 {
        return Err(anyhow::Error::new(std::io::Error::last_os_error())
            .context("failed to read RLIMIT_AS before starting the KVM backend"));
    }
    match KvmAddressSpaceRefusal::for_limit(limit, guest_memory_bytes) {
        Some(refusal) => Err(anyhow::Error::new(refusal)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUEST_MEMORY: usize = 3 << 30;

    /// Only an unlimited soft limit starts the backend; every finite one is
    /// refused, whatever the hard limit, and the refusal names both limits
    /// (in GiB and bytes), the reservations behind the refusal, and both
    /// remedies.
    #[test]
    fn kvm_starts_only_under_an_unlimited_soft_address_space_limit() {
        let limit = |soft, hard| libc::rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };
        let unlimited = libc::RLIM_INFINITY;
        assert_eq!(
            KvmAddressSpaceRefusal::for_limit(limit(unlimited, unlimited), GUEST_MEMORY),
            None
        );
        for (soft, hard, soft_text, hard_text) in [
            (
                16 << 30,
                16 << 30,
                "16 GiB (17179869184 bytes)",
                "16 GiB (17179869184 bytes)",
            ),
            (
                16 << 30,
                unlimited,
                "16 GiB (17179869184 bytes)",
                "unlimited",
            ),
            (
                1 << 40,
                unlimited,
                "1024 GiB (1099511627776 bytes)",
                "unlimited",
            ),
            (
                3537227776,
                unlimited,
                "3.29 GiB (3537227776 bytes)",
                "unlimited",
            ),
            (0, 0, "0 GiB (0 bytes)", "0 GiB (0 bytes)"),
        ] {
            let refusal = KvmAddressSpaceRefusal::for_limit(limit(soft, hard), GUEST_MEMORY)
                .expect("a finite soft limit is refused");
            let text = refusal.to_string();
            assert!(
                text.starts_with(&format!(
                    "kvm_address_space_limit: --backend=kvm refuses to start under a finite \
                     RLIMIT_AS (soft {soft_text}, hard {hard_text}): "
                )),
                "{text}"
            );
            // The reason, in GiB: 3 GiB per guest process, plus 3 GiB per
            // exec in flight.
            assert!(
                text.contains(
                    "reserves 3 GiB (3221225472 bytes) of host address space for every guest \
                     process, plus 3 GiB (3221225472 bytes) more for each exec in flight"
                ),
                "{text}"
            );
            assert!(
                text.contains("Hermit does not raise the limit itself"),
                "{text}"
            );
            // Both remedies: an unlimited limit, and a cgroup bound instead.
            assert!(text.contains("`ulimit -v unlimited`"), "{text}");
            assert!(
                text.contains("bound its memory with a cgroup instead (memory.max"),
                "{text}"
            );
            assert!(text.ends_with("the guest has not started"), "{text}");
        }
    }
}
