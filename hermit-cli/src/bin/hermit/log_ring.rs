/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--log-ring SIZE --log-ring-file FILE`: a flight recorder for Hermit's log.
//!
//! WHY. A full `--log=info` log of a real build step is large (817 MB for one
//! 116-package dnf transaction) and nobody turns it on for every build, so
//! when a build that ran without it turns out not to reproduce, the evidence
//! is gone. The ring keeps only the newest SIZE bytes of the INFO log, so it
//! can stay on in every build, and the newest records are what explains how a
//! run ended.
//!
//! HOW. FILE is a fixed-size file mapped shared into Hermit: a 64-byte header
//! and a SIZE-byte circular data area. Each tracing record is copied into the
//! data area as `[u32 length][bytes]` and the header's positions are updated
//! after it. Because the bytes are in the page cache as they are written, the
//! file holds the newest records however Hermit ends: a normal exit, an error,
//! a panic, `_exit` (which the container processes use), or SIGKILL. Nothing
//! has to run at exit. A record being copied when the process died is not
//! counted, because the header's end position moves only after the copy.
//!
//! READING. `hermit log-diff FILE` decodes a ring file and prints its records
//! as a log; `hermit log-diff A B` compares two ring files, or a ring file and
//! an ordinary log. The decoded log starts with a DEBUG record naming how many
//! older records the ring had dropped.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// The first bytes of a ring file.
pub(crate) const MAGIC: &[u8; 8] = b"HRMRING1";
/// Bytes before the data area.
const HEADER: usize = 64;
const CAPACITY_AT: usize = 8;
/// Byte position, counted from the ring's creation, of the oldest record kept.
const OLDEST_AT: usize = 16;
/// Byte position just past the newest complete record.
const END_AT: usize = 24;
/// How many records were dropped to make room.
const DROPPED_AT: usize = 32;
/// Each record's length prefix.
const LEN: u64 = 4;
/// The smallest data area: room for a record of a useful size.
pub(crate) const MIN_CAPACITY: u64 = 4096;

/// The writer side: a mapped ring file and where its records start.
pub(crate) struct LogRing {
    base: *mut u8,
    capacity: u64,
    /// The start position of every record still in the data area, oldest first.
    starts: VecDeque<u64>,
    end: u64,
    dropped: u64,
}

// SAFETY: the mapping is owned by this value and only reached through `&mut
// self`; the `Mutex` in `RingWriter` serializes writers.
unsafe impl Send for LogRing {}

impl LogRing {
    /// Size `file` to hold `capacity` bytes of records, map it, and write an
    /// empty ring's header.
    pub(crate) fn create(file: &File, capacity: u64) -> io::Result<Self> {
        if capacity < MIN_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--log-ring must be at least {MIN_CAPACITY} bytes"),
            ));
        }
        let length = HEADER as u64 + capacity;
        file.set_len(0)?;
        file.set_len(length)?;
        // SAFETY: a fresh shared mapping of a file descriptor we own, of the
        // length just set; it is unmapped only in `Drop`.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ring = Self {
            base: base.cast(),
            capacity,
            starts: VecDeque::new(),
            end: 0,
            dropped: 0,
        };
        // SAFETY: the header lies inside the mapping.
        unsafe { std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), ring.base, MAGIC.len()) };
        ring.word(CAPACITY_AT).store(capacity, Ordering::Release);
        Ok(ring)
    }

    fn word(&self, at: usize) -> &AtomicU64 {
        // SAFETY: `at` is one of the header's 8-byte aligned slots; the mapping
        // is page aligned and outlives `self`.
        unsafe { &*(self.base.add(at).cast::<AtomicU64>()) }
    }

    fn oldest(&self) -> u64 {
        self.starts.front().copied().unwrap_or(self.end)
    }

    /// Copy `bytes` into the data area at byte position `at`, wrapping.
    fn put(&mut self, at: u64, bytes: &[u8]) {
        let mut offset = (at % self.capacity) as usize;
        let mut rest = bytes;
        while !rest.is_empty() {
            let room = self.capacity as usize - offset;
            let (now, later) = rest.split_at(room.min(rest.len()));
            // SAFETY: `offset + now.len() <= capacity`, inside the data area.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    now.as_ptr(),
                    self.base.add(HEADER + offset),
                    now.len(),
                )
            };
            rest = later;
            offset = 0;
        }
    }

    /// Append one record, dropping the oldest records it needs room from. A
    /// record longer than the data area keeps its last bytes.
    pub(crate) fn push(&mut self, record: &[u8]) {
        let most = (self.capacity - LEN) as usize;
        let record = &record[record.len().saturating_sub(most)..];
        let need = LEN + record.len() as u64;
        while self.end + need - self.oldest() > self.capacity {
            self.starts.pop_front();
            self.dropped += 1;
        }
        // Publish the new oldest record BEFORE overwriting the dropped ones, so
        // a reader of a process killed mid-copy never decodes overwritten bytes.
        self.word(OLDEST_AT).store(self.oldest(), Ordering::Release);
        self.word(DROPPED_AT).store(self.dropped, Ordering::Release);
        let start = self.end;
        self.put(start, &(record.len() as u32).to_le_bytes());
        self.put(start + LEN, record);
        self.starts.push_back(start);
        self.end = start + need;
        if self.starts.len() == 1 {
            self.word(OLDEST_AT).store(start, Ordering::Release);
        }
        self.word(END_AT).store(self.end, Ordering::Release);
    }
}

impl Drop for LogRing {
    fn drop(&mut self) {
        // SAFETY: the mapping created in `create`, of the same length.
        unsafe { libc::munmap(self.base.cast(), HEADER + self.capacity as usize) };
    }
}

/// A tracing writer that appends each write, one formatted record, to a ring.
#[derive(Clone)]
pub(crate) struct RingWriter(Arc<Mutex<LogRing>>);

impl RingWriter {
    pub(crate) fn new(ring: LogRing) -> Self {
        Self(Arc::new(Mutex::new(ring)))
    }
}

impl Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The records of a ring file, oldest first, after a DEBUG record that says
/// how many older ones were dropped; `None` when `bytes` is not a ring file.
pub(crate) fn decode(bytes: &[u8]) -> Option<io::Result<Vec<u8>>> {
    if !bytes.starts_with(MAGIC) {
        return None;
    }
    Some(decode_ring(bytes))
}

fn decode_ring(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("ring file: {what}"));
    if bytes.len() < HEADER {
        return Err(bad("shorter than its header"));
    }
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let (capacity, oldest, end, dropped) = (
        word(CAPACITY_AT),
        word(OLDEST_AT),
        word(END_AT),
        word(DROPPED_AT),
    );
    if capacity < MIN_CAPACITY || bytes.len() as u64 != HEADER as u64 + capacity {
        return Err(bad("its size does not match its capacity"));
    }
    if oldest > end || end - oldest > capacity {
        return Err(bad("its positions are inconsistent"));
    }
    let data = &bytes[HEADER..];
    let get = |at: u64, len: u64| -> Vec<u8> {
        (0..len)
            .map(|i| data[((at + i) % capacity) as usize])
            .collect()
    };
    let mut records = Vec::new();
    let mut count = 0u64;
    let mut at = oldest;
    while at < end {
        let len = u32::from_le_bytes(get(at, LEN).try_into().unwrap()) as u64;
        if at + LEN + len > end {
            return Err(bad("a record runs past the end"));
        }
        records.extend(get(at + LEN, len));
        at += LEN + len;
        count += 1;
    }
    let mut out = format!(
        "1970-01-01T00:00:00.000000Z DEBUG hermit::log_ring: --log-ring file: {count} records, {} bytes of {capacity}; {dropped} older records dropped\n",
        end - oldest
    )
    .into_bytes();
    out.extend(records);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: u64) -> (tempfile::NamedTempFile, LogRing) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let ring = LogRing::create(file.as_file(), capacity).unwrap();
        (file, ring)
    }

    fn records(decoded: &[u8]) -> Vec<String> {
        String::from_utf8(decoded.to_vec())
            .unwrap()
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect()
    }

    /// The file holds the newest records, whole and in order, without any
    /// call at exit: the decoder reads what the mapping wrote.
    #[test]
    fn the_file_keeps_the_newest_whole_records_in_order() {
        let (file, mut ring) = ring(MIN_CAPACITY);
        let mut written = Vec::new();
        for i in 0..2000 {
            let record = format!("record {i:05} {}\n", "x".repeat(i % 37));
            ring.push(record.as_bytes());
            written.push(record.trim_end_matches('\n').to_string());
        }
        let decoded = decode(&std::fs::read(file.path()).unwrap())
            .unwrap()
            .unwrap();
        let kept = records(&decoded);
        assert!(kept.len() > 10 && kept.len() < 2000, "{}", kept.len());
        assert_eq!(kept, written[written.len() - kept.len()..]);
        let header = String::from_utf8(decoded).unwrap();
        let header = header.lines().next().unwrap();
        assert!(
            header.contains(&format!("{} older records dropped", 2000 - kept.len())),
            "{header}"
        );
        let used: u64 = kept.iter().map(|r| LEN + r.len() as u64 + 1).sum();
        assert!(used <= MIN_CAPACITY, "{used}");
    }

    /// A record longer than the data area keeps its last bytes, and a file
    /// that is not a ring is left to the ordinary reader.
    #[test]
    fn oversized_records_keep_their_tail_and_other_files_are_not_rings() {
        let (file, mut ring) = ring(MIN_CAPACITY);
        let long = format!("{}END\n", "y".repeat(10_000));
        ring.push(long.as_bytes());
        let decoded = decode(&std::fs::read(file.path()).unwrap())
            .unwrap()
            .unwrap();
        let kept = records(&decoded);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].ends_with("END"));
        assert_eq!(kept[0].len() as u64, MIN_CAPACITY - LEN - 1);
        assert!(decode(b"2026-10-22T00:00:00Z  INFO detcore: x\n").is_none());
        assert!(LogRing::create(file.as_file(), MIN_CAPACITY - 1).is_err());
    }
}
