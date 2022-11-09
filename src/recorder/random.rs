/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls::Getrandom;
use reverie::syscalls::MemoryAccess;
use reverie::Errno;
use reverie::Guest;

use super::Recorder;
use crate::event::SyscallEvent;

impl Recorder {
    pub(super) async fn handle_getrandom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getrandom,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            result.and_then(|length| {
                let mut buf = vec![0; length as usize];
                let addr = syscall.buf().ok_or(Errno::EFAULT)?;
                guest.memory().read_exact(addr, &mut buf)?;
                Ok(SyscallEvent::Bytes(buf))
            }),
        );

        result
    }
}
