/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Miscellaneous virtualized syscalls.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use chrono::DateTime;
use chrono::Local;
use rand::RngExt as _;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

use crate::consts::DEFAULT_HOSTNAME;
use crate::detlog;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

fn from_str(s: &str) -> [i8; 65] {
    let mut ret: [i8; 65] = [0; 65];
    for (i, ch) in s.bytes().take(64).enumerate() {
        ret[i] = ch as i8;
    }
    ret
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Fill guest memory from the deterministic PRNG owned by the current thread.
    pub(super) fn fill_random_bytes<G: Guest<Self>>(
        &self,
        guest: &mut G,
        remote_buf: AddrMut<u8>,
        len: usize,
        source: &str,
    ) -> Result<usize, Error> {
        let word_size = std::mem::size_of::<u64>();
        let word_count = len / word_size + usize::from(!len.is_multiple_of(word_size));
        let mut local_words = vec![0_u64; word_count];
        // safeptrace's 8-byte write fast path currently requires an aligned source buffer.
        let local_buf =
            unsafe { std::slice::from_raw_parts_mut(local_words.as_mut_ptr().cast::<u8>(), len) };
        guest.thread_state_mut().thread_prng().fill(local_buf);
        let n = guest.memory().write(remote_buf, local_buf)?;
        if cfg!(debug_assertions) {
            let mut hasher = DefaultHasher::new();
            Hash::hash_slice(local_buf, &mut hasher);
            detlog!(
                "[dtid {}] USER RAND [{}] Filled guest memory with {} random bytes, hash of bytes: {}",
                guest.thread_state().dettid,
                source,
                n,
                hasher.finish()
            );
        }
        Ok(n)
    }

    /// uname syscall
    pub async fn handle_uname<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Uname,
    ) -> Result<i64, Error> {
        let ret = self.record_or_replay(guest, call).await?;
        if let Some(buf) = call.buf() {
            let mut un = guest.memory().read_value(buf)?;
            let epoch: DateTime<Local> = guest.config().epoch.into();

            if !guest.config().has_uts_namespace {
                // FIXME: It should be possible to remove this once all tests
                // are also using namespaces.
                un.nodename = from_str(DEFAULT_HOSTNAME);
                un.domainname = from_str(DEFAULT_HOSTNAME.split('.').next_back().unwrap_or(""));
            }

            un.release = from_str("5.2.0");
            un.version = from_str(&format!("#1 SMP {}", epoch.format("%a %b %d %T %Z %Y")));
            guest.memory().write_value(buf, &un)?;
        }

        Ok(ret)
    }

    /// getrandom system call
    pub async fn handle_getrandom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrandom,
    ) -> Result<i64, Error> {
        let buf = call.buf().ok_or(Errno::EFAULT)?;

        let n = self.fill_random_bytes(guest, buf, call.buflen(), "getrandom")?;
        Ok(n as i64)
    }

    /// setsid system call
    pub async fn handle_setsid<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Setsid,
    ) -> Result<i64, Error> {
        let res = guest.inject(call).await?;

        // task is trying to become a daemon process. for more details
        // see: https://notes.shichao.io/apue/ch13/
        if guest.config().kill_daemons {
            guest.daemonize().await;
        }
        Ok(res)
    }

    /// membarrier (system call).
    ///
    /// `membarrier(2)` issues process-wide memory barriers so that userspace can
    /// use asymmetric fences (e.g. CPython's QSBR, RCU-style reclamation).
    /// Detcore serializes all guest threads onto a single logical CPU with a
    /// total memory order, so any requested barrier is *already* satisfied and
    /// every command is a deterministic no-op. For `MEMBARRIER_CMD_QUERY` we
    /// report the set of commands we emulate so the guest stays on this
    /// controlled path instead of a host-dependent fallback; every other command
    /// returns success without doing anything.
    pub async fn handle_membarrier<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Membarrier,
    ) -> Result<i64, Error> {
        // Values from <linux/membarrier.h>.
        const MEMBARRIER_CMD_QUERY: i32 = 0;
        const MEMBARRIER_CMD_GLOBAL: i32 = 1 << 0;
        const MEMBARRIER_CMD_GLOBAL_EXPEDITED: i32 = 1 << 1;
        const MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED: i32 = 1 << 2;
        const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
        const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;
        const SUPPORTED: i32 = MEMBARRIER_CMD_GLOBAL
            | MEMBARRIER_CMD_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED
            | MEMBARRIER_CMD_PRIVATE_EXPEDITED
            | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED;

        let cmd = call.cmd();
        if cmd == MEMBARRIER_CMD_QUERY {
            detlog!(
                "[dtid {}] membarrier(QUERY) => reporting emulated commands {:#x}",
                guest.thread_state().dettid,
                SUPPORTED,
            );
            Ok(SUPPORTED as i64)
        } else {
            detlog!(
                "[dtid {}] membarrier(cmd={}) no-op (threads are serialized on one CPU)",
                guest.thread_state().dettid,
                cmd,
            );
            Ok(0)
        }
    }

    /// getcpu system call
    pub async fn handle_getcpu<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getcpu,
    ) -> Result<i64, Error> {
        // Always set the CPU to 0.
        if let Some(cpu) = call.cpu() {
            guest.memory().write_value(cpu, &0)?;
        }

        // Always set the NUMA node to 0.
        if let Some(node) = call.node() {
            guest.memory().write_value(node, &0)?;
        }

        Ok(0)
    }
}
