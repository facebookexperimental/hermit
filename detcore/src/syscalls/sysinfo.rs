/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::syscalls;
use reverie::syscalls::MemoryAccess;
use reverie::Error;
use reverie::Guest;

use crate::tool_global::thread_observe_time;
use crate::Detcore;
use crate::RecordOrReplay;

const MB: u64 = 1024 * 1024;

impl<T: RecordOrReplay> Detcore<T> {
    /// handle sysinfo syscall
    pub async fn handle_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Sysinfo,
    ) -> Result<i64, Error> {
        let sys_info = self.collect_sysinfo(guest).await?;
        let mut memory = guest.memory();

        if let Some(info_addr) = call.info() {
            memory.write_value(info_addr, &sys_info.into())?;
        }
        Ok(0)
    }

    async fn calculate_uptime<G: Guest<Self>>(&self, guest: &mut G) -> Result<u64, Error> {
        let global_time = thread_observe_time(guest).await;
        Ok(self.cfg.sysinfo_uptime_offset + global_time.as_secs()
            - crate::types::DetTime::new(&self.cfg).as_nanos().as_secs())
    }

    async fn collect_sysinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<syscalls::SysInfo, Error> {
        Ok(syscalls::SysInfo {
            uptime: self.calculate_uptime(guest).await?,
            loads_1: 1,
            loads_5: 1,
            loads_15: 1,
            total_ram: self.cfg.memory,
            free_ram: self.cfg.memory / 2,
            buffer_ram: MB,
            shared_ram: MB,
            total_swap: 0,
            free_swap: 0,
            procs: 1,
            total_high: 0,
            free_high: 0,
            mem_unit: 1,
        })
    }
}
