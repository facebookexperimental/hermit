// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use reverie::syscalls::Addr;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;
use reverie::Errno;
use reverie::Guest;

use super::Recorder;
use crate::event::MmapEvent;
use crate::event::SyscallEvent;

impl Recorder {
    pub(super) async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
    ) -> Result<i64, Errno> {
        // Let anonymous mappings through. This should already be deterministic
        // because ASLR is disabled.
        if syscall.flags().contains(MapFlags::MAP_ANONYMOUS) {
            return guest.inject(syscall).await;
        }

        let len = syscall.len();

        // Do the injection. We need to record the pointer the mapping is at and
        // all of the bytes contained in the mapping. This is very inefficient
        // for large mappings and should be replaced by just letting the mmap
        // through to the real file.
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            match result {
                Ok(addr) => {
                    let addr = Addr::from_raw(addr as usize).ok_or(Errno::EINVAL)?;

                    // NOTE: We can't use `read_exact` here to slurp up the
                    // memory map bytes. The memory size may be larger than the
                    // physical file size for dynamic libraries. The following
                    // `read` will read up to the physical file length, not the
                    // length specified on the memory map. The left over bytes
                    // that extend past the end of the physical file should be
                    // set to zeros when we replay this `mmap`.
                    let mut buf = vec![0u8; len as usize];
                    let physical_length = guest.memory().read(addr, &mut buf)?;

                    // Don't store more bytes than we need to. When we create
                    // the anonymous map later, the extra bytes will be
                    // initialized to zero automatically.
                    buf.truncate(physical_length);

                    Ok(SyscallEvent::Mmap(MmapEvent {
                        addr: addr.as_raw(),
                        buf,
                    }))
                }
                Err(errno) => Err(errno),
            },
        );

        result
    }
}
