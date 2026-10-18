/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Projection of authoritative runtime intervals from complete maps snapshots.
//! This is shared policy; providers supply ownership facts, never procfs text.

use reverie::RuntimePrivateMemoryRegion;

struct Header {
    fields: [std::ops::Range<usize>; 5],
    start: u64,
    end: u64,
    offset: u64,
    inode: u64,
    range_width: usize,
}

fn number(bytes: &[u8], radix: u32) -> Result<u64, &'static str> {
    if bytes.is_empty()
        || !bytes.iter().all(|byte| match radix {
            16 => byte.is_ascii_hexdigit(),
            _ => byte.is_ascii_digit(),
        })
    {
        return Err("invalid numeric maps field");
    }
    u64::from_str_radix(
        std::str::from_utf8(bytes).map_err(|_| "non-ASCII maps header")?,
        radix,
    )
    .map_err(|_| "overflowing maps field")
}

fn header(line: &[u8]) -> Result<Header, &'static str> {
    let mut cursor = 0;
    let mut fields = std::array::from_fn(|_| 0..0);
    for field in &mut fields {
        let start = cursor;
        while cursor < line.len() && !line[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if start == cursor {
            return Err("missing maps field");
        }
        *field = start..cursor;
        while cursor < line.len() && matches!(line[cursor], b' ' | b'\t') {
            cursor += 1;
        }
    }
    let addresses = &line[fields[0].clone()];
    let dash = addresses
        .iter()
        .position(|byte| *byte == b'-')
        .ok_or("missing maps bounds")?;
    let start = number(&addresses[..dash], 16)?;
    let end = number(&addresses[dash + 1..], 16)?;
    if start >= end {
        return Err("reversed or empty maps bounds");
    }
    let permissions = &line[fields[1].clone()];
    if permissions.len() != 4
        || !matches!(permissions[0], b'r' | b'-')
        || !matches!(permissions[1], b'w' | b'-')
        || !matches!(permissions[2], b'x' | b'-')
        || !matches!(permissions[3], b'p' | b's')
    {
        return Err("invalid maps permissions");
    }
    let device = &line[fields[3].clone()];
    let colon = device
        .iter()
        .position(|byte| *byte == b':')
        .ok_or("missing maps device")?;
    number(&device[..colon], 16)?;
    number(&device[colon + 1..], 16)?;
    Ok(Header {
        offset: number(&line[fields[2].clone()], 16)?,
        inode: number(&line[fields[4].clone()], 10)?,
        fields,
        start,
        end,
        range_width: dash,
    })
}

/// Remove only the reported interval union, retaining mixed guest fragments.
/// Run this before requesting inode identities: hidden rows consume no number.
pub(crate) fn project(
    contents: &[u8],
    regions: &[RuntimePrivateMemoryRegion],
) -> Result<Vec<u8>, &'static str> {
    let mut previous_end = 0;
    for region in regions {
        if region.start >= region.end || region.start < previous_end {
            return Err("invalid runtime-private interval union");
        }
        previous_end = region.end;
    }
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        return Err("truncated maps snapshot");
    }
    let mut output = Vec::with_capacity(contents.len());
    let mut previous_end = 0;
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let body = &line[..line.len() - 1];
        let row = header(body)?;
        if row.start < previous_end {
            return Err("overlapping or unordered maps rows");
        }
        previous_end = row.end;
        let mut cursor = row.start;
        for region in regions {
            if region.end <= cursor || region.start >= row.end {
                continue;
            }
            if cursor < region.start {
                append_fragment(&mut output, body, &row, cursor, region.start.min(row.end))?;
            }
            cursor = cursor.max(region.end).min(row.end);
        }
        if cursor < row.end {
            append_fragment(&mut output, body, &row, cursor, row.end)?;
        }
    }
    Ok(output)
}

fn append_fragment(
    output: &mut Vec<u8>,
    body: &[u8],
    row: &Header,
    start: u64,
    end: u64,
) -> Result<(), &'static str> {
    if start == row.start && end == row.end {
        output.extend_from_slice(body);
    } else {
        let offset = if row.inode == 0 && row.offset == 0 {
            0
        } else {
            row.offset
                .checked_add(start - row.start)
                .ok_or("maps fragment offset overflow")?
        };
        output.extend_from_slice(
            format!("{start:0width$x}-{end:0width$x}", width = row.range_width).as_bytes(),
        );
        output.extend_from_slice(&body[row.fields[0].end..row.fields[2].start]);
        output.extend_from_slice(
            format!("{offset:0width$x}", width = row.fields[2].len()).as_bytes(),
        );
        output.extend_from_slice(&body[row.fields[2].end..]);
    }
    output.push(b'\n');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_answer(process: i32, private_start: u64) -> crate::procfs::listing::ProcfsTaskAnswer {
        crate::procfs::listing::ProcfsTaskAnswer {
            exists: true,
            without_mm: false,
            process_incarnation: 1,
            thread_incarnation: None,
            mm: Some(detcore_model::futex::MmId::initial(
                detcore_model::pid::DetTid::from_raw(process),
            )),
            runtime_private_memory: Some(reverie::RuntimePrivateMemory::Ready(
                reverie::RuntimePrivateMemorySnapshot {
                    owner: [10, 20, process as u64, 1],
                    version: 1,
                    regions: vec![RuntimePrivateMemoryRegion {
                        start: private_start,
                        end: private_start + 0x1000,
                    }],
                },
            )),
        }
    }

    #[test]
    fn opened_target_facts_do_not_become_the_readers_or_replacement_images() {
        use crate::procfs::BoundTask;
        use crate::procfs::MapsTarget;
        use crate::procfs::ProcfsFile;
        let task = BoundTask {
            process: 11,
            thread: None,
        };
        let target = target_answer(11, 0x3000);
        let reader = target_answer(22, 0x1000);
        let binding = MapsTarget::from_open(task, &target, &target).unwrap();
        let mut opened = ProcfsFile::from_path(std::path::Path::new("/proc/11/maps")).unwrap();
        opened.bind_maps_target(binding.clone());
        // The description's clone/serialization keeps the old target rather
        // than looking up the PID again in the reader or replacement image.
        let alias: ProcfsFile =
            serde_json::from_slice(&serde_json::to_vec(&opened.clone()).unwrap()).unwrap();
        assert_eq!(alias.maps_target(), Some(binding.clone()));
        let raw = b"1000-2000 rw-p 0 00:00 0 \n3000-4000 rw-p 0 00:00 0 \n";
        assert_eq!(
            project(raw, &binding.private.regions).unwrap(),
            b"1000-2000 rw-p 0 00:00 0 \n"
        );
        let reverie::RuntimePrivateMemory::Ready(reader) = reader.runtime_private_memory.unwrap()
        else {
            unreachable!()
        };
        assert_ne!(
            project(raw, &reader.regions).unwrap(),
            project(raw, &binding.private.regions).unwrap()
        );
        let mut replacement = target.clone();
        replacement.process_incarnation += 1;
        assert!(MapsTarget::from_open(task, &target, &replacement).is_none());
        replacement = target.clone();
        replacement.mm = Some(
            target
                .mm
                .unwrap()
                .for_exec(detcore_model::pid::DetTid::from_raw(11)),
        );
        assert!(MapsTarget::from_open(task, &target, &replacement).is_none());
        replacement = target.clone();
        replacement.runtime_private_memory = Some(reverie::RuntimePrivateMemory::Unavailable);
        assert!(MapsTarget::from_open(task, &target, &replacement).is_none());
        assert_eq!(alias.maps_target(), Some(binding));
        for (path, expected) in [
            (
                "/proc/self/maps",
                BoundTask {
                    process: 11,
                    thread: None,
                },
            ),
            (
                "/proc/thread-self/maps",
                BoundTask {
                    process: 11,
                    thread: Some(12),
                },
            ),
            (
                "/proc/11/task/12/maps",
                BoundTask {
                    process: 11,
                    thread: Some(12),
                },
            ),
        ] {
            assert_eq!(
                crate::procfs::maps_task_at(std::path::Path::new(path), 11, 12),
                Some(expected)
            );
        }
    }

    #[test]
    fn private_commitment_splits_do_not_change_bytes_or_inode_requests() {
        let before = b"1000-2000 r-xp 00000000 08:01 7 /guest\n3000-6000 ---p 00000000 00:00 0 \n7000-8000 rw-p 00001000 08:01 9 /data\n";
        let after = b"1000-2000 r-xp 00000000 08:01 7 /guest\n3000-4000 rw-p 00000000 00:00 0 \n4000-5000 ---p 00000000 08:02 999 /private\n5000-6000 rw-p 00000000 00:00 0 \n7000-8000 rw-p 00001000 08:01 9 /data\n";
        let private = [RuntimePrivateMemoryRegion {
            start: 0x3000,
            end: 0x6000,
        }];
        let a = project(before, &private).unwrap();
        let b = project(after, &private).unwrap();
        assert_eq!(a, b);
        let identities = |bytes: &[u8]| {
            std::str::from_utf8(bytes)
                .unwrap()
                .lines()
                .filter_map(crate::procfs::mapping_header_identity)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            identities(&a),
            vec![(libc::makedev(8, 1), 7), (libc::makedev(8, 1), 9)]
        );
        assert_eq!(identities(&a), identities(&b));
    }

    #[test]
    fn mixed_rows_preserve_guest_offsets_and_raw_pathnames() {
        let raw = b"1000-5000 rw-p 00002000 08:01 7 /a \xff name (deleted)\n6000-a000 rw-p 00000000 00:00 0 \n";
        let private = [
            RuntimePrivateMemoryRegion {
                start: 0x2000,
                end: 0x4000,
            },
            RuntimePrivateMemoryRegion {
                start: 0x7000,
                end: 0x9000,
            },
        ];
        assert_eq!(project(raw, &private).unwrap(), b"1000-2000 rw-p 00002000 08:01 7 /a \xff name (deleted)\n4000-5000 rw-p 00005000 08:01 7 /a \xff name (deleted)\n6000-7000 rw-p 00000000 00:00 0 \n9000-a000 rw-p 00000000 00:00 0 \n");
    }

    #[test]
    fn projection_refuses_malformed_input_even_when_hidden() {
        let all = [RuntimePrivateMemoryRegion {
            start: 1,
            end: u64::MAX,
        }];
        for raw in [
            &b"1000-2000 rw-p 0 00:00 0"[..],
            &b"2000-1000 rw-p 0 00:00 0\n"[..],
            &b"1000-2000 rw-p 10000000000000000 00:00 0\n"[..],
            &b"1000-2000 rw-p 0 00:00 nope\n"[..],
        ] {
            assert!(project(raw, &all).is_err());
        }
        assert!(project(b"", &[RuntimePrivateMemoryRegion { start: 2, end: 1 }]).is_err());
        assert!(
            project(
                b"1000-3000 rw-p ffffffffffffffff 08:01 1 /file\n",
                &[RuntimePrivateMemoryRegion {
                    start: 0x1000,
                    end: 0x2000
                }]
            )
            .is_err()
        );
    }
}
