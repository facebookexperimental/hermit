//! The application's actual initial auxiliary vector at native entry.
//!
//! The loader can rewrite the application stack while the kernel's saved
//! vector still describes the loader. Preserve the maintained bounded walk,
//! fault-safe memory mechanism, then the same kernel-vector fallback.

use std::io;

use reverie_inguest::guest::native_services::MemoryReader;

type Entries = Vec<(libc::c_ulong, libc::c_ulong)>;

pub(super) fn read(reader: MemoryReader) -> io::Result<Entries> {
    application(reader)
        .or_else(|| kernel(&std::fs::read("/proc/self/auxv").ok()?))
        .ok_or_else(|| io::Error::other("failed to read the application's auxiliary vector"))
}

fn application(reader: MemoryReader) -> Option<Entries> {
    let stat = std::fs::read("/proc/self/stat").ok()?;
    let fields = &stat[stat.iter().rposition(|&byte| byte == b')')? + 2..];
    let start = std::str::from_utf8(fields.split(|&byte| byte == b' ').nth(28 - 3)?)
        .ok()?
        .parse::<u64>()
        .ok()?;
    after_stack(start, |address| {
        let mut word = 0_u64;
        let status = unsafe {
            reader(
                address as usize,
                (&mut word as *mut u64).cast(),
                size_of::<u64>(),
            )
        };
        (status != 0).then_some(word)
    })
}

fn kernel(bytes: &[u8]) -> Option<Entries> {
    let mut entries = Vec::new();
    for pair in bytes.chunks(16) {
        let pair: &[u8; 16] = pair.try_into().ok()?;
        let (key, value) = pair.split_at(8);
        let key = u64::from_ne_bytes(key.try_into().unwrap());
        let value = u64::from_ne_bytes(value.try_into().unwrap());
        if key == 0 && value == 0 {
            return Some(entries);
        }
        entries.push((key, value));
    }
    None
}

fn after_stack(start: u64, read_word: impl Fn(u64) -> Option<u64>) -> Option<Entries> {
    // Same corruption bounds as the maintained native Guest. These are not
    // new limits on Linux's accepted argument/environment contents.
    const MOST_POINTERS: u64 = 1 << 22;
    const MOST_ENTRIES: u64 = 256;
    const WORD: u64 = size_of::<u64>() as u64;
    if start == 0 {
        return None;
    }
    let argc = read_word(start)?;
    if argc >= MOST_POINTERS {
        return None;
    }
    let argv_end = start.checked_add((1 + argc) * WORD)?;
    if read_word(argv_end)? != 0 {
        return None;
    }
    let mut cursor = argv_end + WORD;
    let mut pointers = 0;
    while read_word(cursor)? != 0 {
        pointers += 1;
        if pointers == MOST_POINTERS {
            return None;
        }
        cursor = cursor.checked_add(WORD)?;
    }
    cursor = cursor.checked_add(WORD)?;
    let mut entries = Vec::new();
    loop {
        let key = read_word(cursor)?;
        if key == libc::AT_NULL {
            return Some(entries);
        }
        entries.push((key, read_word(cursor.checked_add(WORD)?)?));
        if entries.len() as u64 == MOST_ENTRIES {
            return None;
        }
        cursor = cursor.checked_add(2 * WORD)?;
    }
}
