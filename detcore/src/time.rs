// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! detcore time/timespec

use std::cmp::Ordering;
use std::hash::Hash;
use std::hash::Hasher;

use serde::Deserialize;
use serde::Serialize;

/// timespec, but with serialize/deserialize instance
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Timespec {
    /// seconds
    pub tv_sec: i64,
    /// nanoseconds
    pub tv_nsec: i64,
}

impl PartialEq for Timespec {
    fn eq(&self, other: &Timespec) -> bool {
        self.tv_sec == other.tv_sec && self.tv_nsec == other.tv_nsec
    }
}

impl Eq for Timespec {}

impl PartialOrd for Timespec {
    fn partial_cmp(&self, other: &Timespec) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timespec {
    fn cmp(&self, other: &Timespec) -> Ordering {
        let me = (self.tv_sec, self.tv_nsec);
        let other = (other.tv_sec, other.tv_nsec);
        me.cmp(&other)
    }
}

impl Hash for Timespec {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.tv_sec.hash(state);
        self.tv_nsec.hash(state);
    }
}

impl From<&libc::timespec> for Timespec {
    fn from(tp: &libc::timespec) -> Self {
        Timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
        }
    }
}

impl From<&Timespec> for libc::timespec {
    fn from(tp: &Timespec) -> Self {
        libc::timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
        }
    }
}

impl From<libc::timespec> for Timespec {
    fn from(tp: libc::timespec) -> Self {
        Timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
        }
    }
}

impl From<Timespec> for libc::timespec {
    fn from(tp: Timespec) -> Self {
        libc::timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
        }
    }
}

impl From<&libc::statx_timestamp> for Timespec {
    fn from(tp: &libc::statx_timestamp) -> Self {
        Timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
        }
    }
}

impl From<&Timespec> for libc::statx_timestamp {
    fn from(tp: &Timespec) -> Self {
        libc::statx_timestamp {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
            __statx_timestamp_pad1: [0],
        }
    }
}

impl From<libc::statx_timestamp> for Timespec {
    fn from(tp: libc::statx_timestamp) -> Self {
        Timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
        }
    }
}

impl From<Timespec> for libc::statx_timestamp {
    fn from(tp: Timespec) -> Self {
        libc::statx_timestamp {
            tv_sec: tp.tv_sec as _,
            tv_nsec: tp.tv_nsec as _,
            __statx_timestamp_pad1: [0],
        }
    }
}

impl From<&libc::timeval> for Timespec {
    fn from(tp: &libc::timeval) -> Self {
        Timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: 1000 * (tp.tv_usec as i64),
        }
    }
}

impl From<&Timespec> for libc::timeval {
    fn from(tp: &Timespec) -> Self {
        libc::timeval {
            tv_sec: tp.tv_sec as _,
            tv_usec: (tp.tv_nsec / 1000) as _,
        }
    }
}

impl From<libc::timeval> for Timespec {
    fn from(tp: libc::timeval) -> Self {
        Timespec {
            tv_sec: tp.tv_sec as _,
            tv_nsec: (1000 * tp.tv_usec) as _,
        }
    }
}

impl From<Timespec> for libc::timeval {
    fn from(tp: Timespec) -> Self {
        libc::timeval {
            tv_sec: tp.tv_sec as _,
            tv_usec: (tp.tv_nsec / 1000) as _,
        }
    }
}
