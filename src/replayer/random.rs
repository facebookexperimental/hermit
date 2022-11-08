// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use reverie::syscalls::Getrandom;
use reverie::syscalls::MemoryAccess;
use reverie::Errno;
use reverie::Guest;

use super::Replayer;

impl Replayer {
    pub(super) async fn handle_getrandom<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Getrandom,
    ) -> Result<i64, Errno> {
        let buf = next_event!(guest, Bytes)?;

        assert!(buf.len() <= syscall.buflen());

        // Write out the buffer.
        guest
            .memory()
            .write_exact(syscall.buf().unwrap(), &buf)
            .unwrap();
        Ok(buf.len() as i64)
    }
}
