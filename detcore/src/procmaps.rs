// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use digest::Digest;
pub use procfs::process::MMapPath;
pub use procfs::process::MemoryMap;
use reverie::syscalls::Addr;
use reverie::syscalls::MemoryAccess;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;

fn display_pathname(p: &MMapPath) -> String {
    match p {
        MMapPath::Vdso => String::from("[vsdo]"),
        MMapPath::Stack => String::from("[stack]"),
        MMapPath::TStack(tid) => format!("[tstack:{}]", tid),
        MMapPath::Vvar => String::from("[vvar]"),
        MMapPath::Vsyscall => String::from("[syscalls]"),
        MMapPath::Heap => String::from("[heap]"),
        MMapPath::Other(s) => format!("[other: {}]", s),
        MMapPath::Anonymous => String::from("[annonymous]"),
        MMapPath::Path(s) => s.display().to_string(),
    }
}

pub fn display(map: &MemoryMap) -> String {
    format!(
        "{:#x}-{:#x} {} {:x} {:x}:{:x} {} {}",
        map.address.0,
        map.address.1,
        map.perms,
        map.offset,
        map.dev.0,
        map.dev.1,
        map.inode,
        display_pathname(&map.pathname)
    )
}

fn map_error(err: procfs::ProcError) -> reverie::Error {
    match err {
        procfs::ProcError::Io(err, _) => reverie::Error::Io(err),
        err => reverie::Error::Tool(anyhow::anyhow!(err)),
    }
}

pub fn from_pid<F>(pid: Pid, filter: F) -> Result<Vec<MemoryMap>, reverie::Error>
where
    F: Fn(&MemoryMap) -> bool,
{
    match procfs::process::Process::new(pid.as_raw()) {
        Ok(process) => match process.maps() {
            Ok(mut maps) => {
                maps.retain(filter);
                Ok(maps)
            }
            Err(err) => Err(map_error(err)),
        },
        Err(err) => Err(map_error(err)),
    }
}

pub fn compute_hash<G, T: Tool>(guest: &mut G, map: &MemoryMap) -> Result<Digest, reverie::Error>
where
    G: Guest<T>,
{
    let size = (map.address.1 - map.address.0) as usize;
    let memory = guest.memory();
    let mut buf = vec![0; size];
    let start = Addr::<u8>::from_raw(map.address.0 as usize).unwrap();
    memory.read_values(start, buf.as_mut_slice())?;
    return Ok(Digest::new(buf.as_slice()));
}
