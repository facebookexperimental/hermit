/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Virtualize uname syscall

use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use chrono::DateTime;
use chrono::Local;
use rand::Rng;
use reverie::syscalls;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::Error;
use reverie::Guest;

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
                un.domainname = from_str(DEFAULT_HOSTNAME.split('.').last().unwrap_or(""));
            }

            un.release = from_str("5.2.0");
            un.version = from_str(&format!("#1 SMP {}", epoch.format("%a %b %d %T %Z %Y")));
            guest.memory().write_value(buf, &un)?;
        }

        Ok(ret)
    }

    /// getrandon system call
    pub async fn handle_getrandom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Getrandom,
    ) -> Result<i64, Error> {
        let buf = call.buf().ok_or(Errno::EFAULT)?;

        let mut local_buf: Vec<u8> = vec![0; call.buflen()];
        guest
            .thread_state_mut()
            .thread_prng()
            .fill(local_buf.as_mut_slice());
        let n = guest.memory().write(buf, local_buf.as_slice())?;
        if cfg!(debug_assertions) {
            let mut hasher = DefaultHasher::new();
            Hash::hash_slice(local_buf.as_slice(), &mut hasher);
            detlog!(
                "[dtid {}] USER RAND [getrandom] Filled guest memory with {} random bytes, hash of bytes: {}",
                guest.thread_state().dettid,
                n,
                hasher.finish()
            );
        }

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
