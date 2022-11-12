/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! DetStat used by stat families

use std::fmt;

use reverie::syscalls::StatxMask;
use reverie::syscalls::Timespec;
use serde::Deserialize;
use serde::Serialize;

bitflags! {
    /// stx_attributes from statx, see linux/stat.h
    #[derive(Serialize, Deserialize)]
    pub struct StatxAttributes: u64 {
        /// [I] File is compressed by the fs
        const STATX_ATTR_COMPRESSED = 0x4;
        /// [I] File is marked immutable
        const STATX_ATTR_IMMUTABLE = 0x00000010;
        /// [I] File is append-only
        const STATX_ATTR_APPEND	= 0x00000020;
        /// [I] File is not to be dumped
        const STATX_ATTR_NODUMP	= 0x00000040;
        /// [I] File requires key to decrypt in fs
        const STATX_ATTR_ENCRYPTED = 0x00000800;
        /// Dir: Automount trigger
        const STATX_ATTR_AUTOMOUNT = 0x00001000;
        /// Root of a mount
        const STATX_ATTR_MOUNT_ROOT	= 0x00002000;
        /// [I] Verity protected file
        const STATX_ATTR_VERITY	= 0x00100000;
        /// File is currently in DAX state
        const STATX_ATTR_DAX = 0x00200000;
    }
}

/// Detcore stat/statx
#[allow(missing_docs)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DetStat {
    /// stx_mask
    pub mask: StatxMask,
    /// stx_mode or st_mode
    pub mode: u32,
    /// uid
    pub uid: u32,
    /// gid
    pub gid: u32,
    /// stx_attributes
    pub attributes: StatxAttributes,
    /// stx_attributes_mask
    pub attributes_mask: StatxAttributes,
    /// stx_dev (maj << 32 | min)
    pub dev: u64,
    /// stx_rdev (maj << 32 | min)
    pub rdev: u64,
    /// inode
    pub inode: u64,
    /// stx_nlink
    pub nlink: u64,
    /// stx_size
    pub size: i64,
    /// stx_blksize,
    pub blksize: i64,
    /// stx_blocks
    pub blocks: i64,
    /// atime
    pub atime: Timespec,
    /// btime
    pub btime: Timespec,
    /// ctime
    pub ctime: Timespec,
    /// mtime
    pub mtime: Timespec,
}

impl Default for DetStat {
    fn default() -> Self {
        let statx: libc::statx = unsafe { std::mem::zeroed() };
        let mut stat: Self = statx.into();
        stat.mask = StatxMask::STATX_BASIC_STATS;
        stat.nlink = 1;
        stat
    }
}

impl fmt::Debug for DetStat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("DetStat")
            .field("mode", &self.mode)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("dev", &self.dev)
            .field("rdev", &self.rdev)
            .field("inode", &self.inode)
            .field("nlink", &self.nlink)
            .field("size", &self.size)
            .field("blksize", &self.blksize)
            .field("blocks", &self.blocks)
            .field("atime", &self.atime)
            .field("btime", &self.btime)
            .field("ctime", &self.ctime)
            .field("mtime", &self.mtime)
            .finish()
    }
}

impl From<&libc::stat> for DetStat {
    fn from(st: &libc::stat) -> Self {
        DetStat {
            mask: StatxMask::STATX_BASIC_STATS,
            mode: st.st_mode,
            uid: st.st_uid,
            gid: st.st_gid,
            attributes: StatxAttributes::empty(),
            attributes_mask: StatxAttributes::empty(),
            dev: st.st_dev,
            rdev: st.st_rdev,
            inode: st.st_ino,
            nlink: st.st_nlink,
            size: st.st_size,
            blksize: st.st_blksize,
            blocks: st.st_blocks,
            atime: Timespec {
                tv_sec: st.st_atime as _,
                tv_nsec: st.st_atime_nsec as _,
            },
            btime: Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            ctime: Timespec {
                tv_sec: st.st_ctime as _,
                tv_nsec: st.st_ctime_nsec as _,
            },
            mtime: Timespec {
                tv_sec: st.st_mtime as _,
                tv_nsec: st.st_mtime_nsec as _,
            },
        }
    }
}
impl From<libc::stat> for DetStat {
    fn from(st: libc::stat) -> Self {
        (&st).into()
    }
}

impl From<&DetStat> for libc::stat {
    fn from(st: &DetStat) -> Self {
        // libc::stat doesn't provide `impl Default`
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_dev = st.dev;
        stat.st_ino = st.inode;
        stat.st_nlink = st.nlink;
        stat.st_mode = st.mode;
        stat.st_uid = st.uid;
        stat.st_gid = st.gid;
        stat.st_rdev = st.rdev;
        stat.st_size = st.size;
        stat.st_blksize = st.blksize;
        stat.st_blocks = st.blocks;
        stat.st_atime = st.atime.tv_sec;
        stat.st_atime_nsec = st.atime.tv_nsec;
        stat.st_mtime = st.mtime.tv_sec;
        stat.st_mtime_nsec = st.mtime.tv_nsec;
        stat.st_ctime = st.ctime.tv_sec;
        stat.st_ctime_nsec = st.ctime.tv_nsec;
        stat
    }
}

impl From<DetStat> for libc::stat {
    fn from(st: DetStat) -> Self {
        (&st).into()
    }
}

impl From<&libc::statx> for DetStat {
    fn from(st: &libc::statx) -> Self {
        DetStat {
            mask: StatxMask::from_bits_truncate(st.stx_mask),
            mode: st.stx_mode as _,
            uid: st.stx_uid,
            gid: st.stx_gid,
            attributes: StatxAttributes::from_bits_truncate(st.stx_attributes),
            attributes_mask: StatxAttributes::from_bits_truncate(st.stx_attributes_mask),
            dev: (st.stx_dev_major as u64) << 32 | st.stx_dev_minor as u64,
            rdev: (st.stx_rdev_major as u64) << 32 | st.stx_rdev_minor as u64,
            inode: st.stx_ino,
            nlink: st.stx_nlink as u64,
            size: st.stx_size as i64,
            blksize: st.stx_blksize as i64,
            blocks: st.stx_blocks as i64,
            atime: st.stx_atime.into(),
            btime: st.stx_btime.into(),
            ctime: st.stx_ctime.into(),
            mtime: st.stx_mtime.into(),
        }
    }
}

impl From<libc::statx> for DetStat {
    fn from(st: libc::statx) -> Self {
        (&st).into()
    }
}

impl From<&DetStat> for libc::statx {
    fn from(st: &DetStat) -> Self {
        // libc::statx doesn't provide `impl Default`
        let mut statx: libc::statx = unsafe { std::mem::zeroed() };
        statx.stx_mask = st.mask.bits();
        statx.stx_blksize = st.blksize as u32;
        statx.stx_attributes = st.attributes.bits();
        statx.stx_nlink = st.nlink as u32;
        statx.stx_uid = st.uid;
        statx.stx_gid = st.gid;
        statx.stx_mode = st.mode as u16;
        statx.stx_ino = st.inode;
        statx.stx_size = st.size as u64;
        statx.stx_blocks = st.blocks as u64;
        statx.stx_attributes_mask = st.attributes_mask.bits();
        statx.stx_atime = st.atime.into();
        statx.stx_btime = st.btime.into();
        statx.stx_ctime = st.ctime.into();
        statx.stx_mtime = st.mtime.into();
        statx.stx_rdev_major = (st.rdev >> 32) as u32;
        statx.stx_rdev_minor = (st.rdev & 0xffffffffu64) as u32;
        statx.stx_dev_major = (st.dev >> 32) as u32;
        statx.stx_dev_minor = (st.dev & 0xffffffff) as u32;
        statx
    }
}

impl From<DetStat> for libc::statx {
    fn from(st: DetStat) -> Self {
        (&st).into()
    }
}
