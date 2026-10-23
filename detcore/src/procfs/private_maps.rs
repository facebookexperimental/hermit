/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Projection of authoritative runtime intervals from complete procfs snapshots.
//! This is shared policy; providers supply ownership facts, never procfs text.

use reverie::RuntimePrivateMemoryRegion;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Format {
    Maps,
    Smaps,
    NumaMaps,
}

impl Format {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Maps => "maps",
            Self::Smaps => "smaps",
            Self::NumaMaps => "numa_maps",
        }
    }

    pub(crate) fn project(
        self,
        contents: &[u8],
        regions: &[RuntimePrivateMemoryRegion],
    ) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Maps => project(contents, regions),
            Self::Smaps => project_smaps(contents, regions),
            Self::NumaMaps => Err("coherent companion Maps capture is required"),
        }
    }
}

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
    validate_regions(regions)?;
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
        for_guest_fragments(&row, regions, |start, end| {
            append_fragment(&mut output, body, &row, start, end)
        })?;
    }
    Ok(output)
}

fn validate_regions(regions: &[RuntimePrivateMemoryRegion]) -> Result<(), &'static str> {
    let mut previous_end = 0;
    for region in regions {
        if region.start >= region.end || region.start < previous_end {
            return Err("invalid runtime-private interval union");
        }
        previous_end = region.end;
    }
    Ok(())
}

fn for_guest_fragments(
    row: &Header,
    regions: &[RuntimePrivateMemoryRegion],
    mut visit: impl FnMut(u64, u64) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let mut cursor = row.start;
    for region in regions {
        if region.end <= cursor || region.start >= row.end {
            continue;
        }
        if cursor < region.start {
            visit(cursor, region.start.min(row.end))?;
        }
        cursor = cursor.max(region.end).min(row.end);
    }
    if cursor < row.end {
        visit(cursor, row.end)?;
    }
    Ok(())
}

/// Project raw NUMA rows using the full extents of a contemporaneous Maps capture.
/// The caller must establish that capture's coherence; matching row starts alone
/// cannot prove it. Apply the existing NUMA sanitizer once, after this succeeds.
pub(crate) fn project_numa(
    maps: &[u8],
    numa: &[u8],
    regions: &[RuntimePrivateMemoryRegion],
) -> Result<Vec<u8>, &'static str> {
    validate_regions(regions)?;
    if !maps.is_empty() && !maps.ends_with(b"\n") {
        return Err("truncated maps snapshot");
    }
    if !numa.is_empty() && !numa.ends_with(b"\n") {
        return Err("truncated numa snapshot");
    }
    let mut mappings = Vec::new();
    let mut previous_end = 0;
    for line in maps.split_inclusive(|byte| *byte == b'\n') {
        let row = header(&line[..line.len() - 1])?;
        if row.start < previous_end {
            return Err("overlapping or unordered maps rows");
        }
        previous_end = row.end;
        mappings.push(row);
    }

    let mut output = Vec::with_capacity(numa.len());
    let mut previous_start = None;
    for line in numa.split_inclusive(|byte| *byte == b'\n') {
        let body = &line[..line.len() - 1];
        let mut fields = body
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let start = number(fields.next().ok_or("missing numa start address")?, 16)
            .map_err(|_| "invalid numa start address")?;
        // Policy and other fields remain opaque on whole guest VMAs, including
        // inputs for which the existing sanitizer deliberately preserves bytes.
        fields.next().ok_or("missing numa policy")?;
        if previous_start.is_some_and(|previous| start <= previous) {
            return Err("duplicate or unordered numa rows");
        }
        previous_start = Some(start);
        let index = mappings
            .binary_search_by_key(&start, |row| row.start)
            .map_err(|_| "numa start has no exact maps match")?;
        let row = &mappings[index];
        let mut guest_end = None;
        for_guest_fragments(row, regions, |start, end| {
            // A suffix or a second fragment would require a different policy
            // lookup address, including a different shmem file offset.
            if start != row.start {
                return Err("mixed numa mapping changes policy lookup start");
            }
            guest_end = Some(end);
            Ok(())
        })?;
        if let Some(end) = guest_end {
            if end != row.end {
                validate_numa_guest_prefix(body)?;
            }
            output.extend_from_slice(line);
        } else {
            // Dropping this row must not erase a whole-view sanitizer fallback
            // and thereby strip accounting from the remaining guest rows.
            numa_without_fallback(body)?;
        }
    }
    Ok(output)
}

fn numa_without_fallback(body: &[u8]) -> Result<&str, &'static str> {
    let text = std::str::from_utf8(body).map_err(|_| "non-UTF8 numa sanitizer fallback")?;
    for field in text.split_whitespace() {
        if super::numa_page_accounting_value(field)
            .is_some_and(|value| value.parse::<u64>().is_err())
        {
            return Err("malformed numa accounting fallback");
        }
    }
    Ok(text)
}

fn validate_numa_guest_prefix(body: &[u8]) -> Result<(), &'static str> {
    let text = numa_without_fallback(body)?;
    let mut fields = text.split_whitespace().skip(1).peekable();
    let policy = fields.next().ok_or("missing numa policy")?;
    // Linux mpol_to_str emits these two multiword policy names. Their flags
    // and node lists stay opaque, just as for a single-word policy: no policy
    // is reconstructed or evaluated at a new address.
    let is_policy_tail = |field: &str, name: &str| {
        field
            .strip_prefix(name)
            .is_some_and(|tail| tail.is_empty() || tail.starts_with('=') || tail.starts_with(':'))
    };
    if policy == "weighted" {
        if !fields
            .next()
            .is_some_and(|field| is_policy_tail(field, "interleave"))
        {
            return Err("mixed numa mapping has unknown policy");
        }
    } else if policy == "prefer"
        && fields
            .peek()
            .is_some_and(|field| is_policy_tail(field, "(many)"))
    {
        fields.next();
    }
    let mut has_file = false;
    for field in fields {
        if matches!(field, "heap" | "stack" | "huge") {
            return Err("mixed numa mapping has region tags");
        }
        if let Some(path) = field.strip_prefix("file=") {
            if path.is_empty() || has_file {
                return Err("mixed numa mapping has unsupported file fields");
            }
            has_file = true;
            continue;
        }
        if super::numa_page_accounting_value(field).is_some() {
            return Err("mixed numa mapping has residency fields");
        }
        if field.starts_with("kernelpagesize_kB=") {
            return Err("mixed numa mapping has page-size presence");
        }
        return Err("mixed numa mapping has unknown fields");
    }
    Ok(())
}

/// Smaps accounting describes a whole VMA. Remove complete private sections,
/// but never invent accounting for the guest fragments of a mixed VMA.
/// Field values stay opaque: the existing sanitizer sees the projected view.
fn project_smaps(
    contents: &[u8],
    regions: &[RuntimePrivateMemoryRegion],
) -> Result<Vec<u8>, &'static str> {
    validate_regions(regions)?;
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        return Err("truncated smaps snapshot");
    }
    let mut output = Vec::with_capacity(contents.len());
    let mut previous_end = 0;
    let mut section: Option<(Header, usize, bool)> = None;
    let mut offset = 0;
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let body = &line[..line.len() - 1];
        if smaps_field(body) {
            match section.as_mut() {
                Some((_, _, has_body)) => *has_body = true,
                None => return Err("smaps field before first mapping header"),
            }
        } else {
            // Any non-field must be a complete header, even in a section
            // that will be hidden. Malformed headers cannot become body.
            let row = header(body)?;
            if row.start < previous_end {
                return Err("overlapping or unordered smaps rows");
            }
            previous_end = row.end;
            if let Some((old, start, has_body)) = section.take() {
                append_smaps_section(
                    &mut output,
                    &contents[start..offset],
                    &old,
                    has_body,
                    regions,
                )?;
            }
            section = Some((row, offset, false));
        }
        offset += line.len();
    }
    if let Some((row, start, has_body)) = section {
        append_smaps_section(&mut output, &contents[start..], &row, has_body, regions)?;
    }
    Ok(output)
}

fn smaps_field(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    let label = &line[..colon];
    label.first().is_some_and(u8::is_ascii_alphabetic)
        && label
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

fn append_smaps_section(
    output: &mut Vec<u8>,
    bytes: &[u8],
    row: &Header,
    has_body: bool,
    regions: &[RuntimePrivateMemoryRegion],
) -> Result<(), &'static str> {
    if !has_body {
        return Err("smaps mapping header has no body");
    }
    let private_bytes = regions
        .iter()
        .map(|region| {
            row.end
                .min(region.end)
                .saturating_sub(row.start.max(region.start))
        })
        .sum::<u64>();
    if private_bytes == 0 {
        output.extend_from_slice(bytes);
    } else if private_bytes != row.end - row.start {
        return Err("mixed ownership has unsupported per-VMA accounting");
    }
    Ok(())
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
    fn smaps_opened_target_keeps_the_same_owner_fact() {
        use crate::procfs::BoundTask;
        use crate::procfs::MapsTarget;
        use crate::procfs::ProcfsFile;

        let answer = target_answer(11, 0x3000);
        for (path, thread) in [
            ("/proc/self/smaps", None),
            ("/proc/thread-self/smaps", Some(12)),
            ("/proc/11/smaps", None),
            ("/proc/11/task/12/smaps", Some(12)),
        ] {
            let task = BoundTask {
                process: 11,
                thread,
            };
            let binding = MapsTarget::from_open(task, &answer, &answer).unwrap();
            let mut opened = ProcfsFile::from_path(std::path::Path::new(path)).unwrap();
            opened.bind_maps_target(binding.clone());
            let alias: ProcfsFile =
                serde_json::from_slice(&serde_json::to_vec(&opened.clone()).unwrap()).unwrap();
            assert_eq!(alias.maps_target(), Some(binding));
            assert_eq!(
                crate::procfs::maps_task_at(std::path::Path::new(path), 11, 12),
                Some(task)
            );
        }
    }

    #[test]
    fn smaps_projection_preserves_guest_sections_and_fallback() {
        let private = [RuntimePrivateMemoryRegion {
            start: 0x3000,
            end: 0x6000,
        }];
        let guest =
            b"1000-2000 r--p 00000000 08:01 7 /guest\nSize: 4 kB\nRss: 3 kB\nVmFlags: rd mr\n";
        let hidden = b"3000-6000 ---p 00000000 08:02 999 /private\nUnknownPrivate: invalid value\n";
        let raw = [guest.as_slice(), hidden.as_slice()].concat();
        let projected = project_smaps(&raw, &private).unwrap();
        assert_eq!(projected, guest);
        let table = std::collections::BTreeMap::from([((libc::makedev(8, 1), 7), (0, 10))]);
        let sanitized = crate::procfs::sanitize_smaps(&projected, &table);
        assert_eq!(sanitized, crate::procfs::sanitize_smaps(guest, &table));
        assert!(
            sanitized
                .windows(b"Rss:\t0 kB".len())
                .any(|w| w == b"Rss:\t0 kB")
        );
        assert!(
            !sanitized
                .windows(b"/private".len())
                .any(|w| w == b"/private")
        );
        let identities: Vec<_> = std::str::from_utf8(&projected)
            .unwrap()
            .lines()
            .filter_map(crate::procfs::mapping_header_identity)
            .collect();
        assert_eq!(identities, vec![(libc::makedev(8, 1), 7)]);

        for guest in [
            b"1000-2000 r--p 0 08:01 7 /guest\nUnknownGuest: preserved\n".as_slice(),
            b"1000-2000 r--p 0 08:01 7 /guest\nRss: invalid\n".as_slice(),
            b"1000-2000 r--p 0 08:01 7 /guest-\xff\nRss: 7 kB\n".as_slice(),
        ] {
            let raw = [guest, hidden.as_slice()].concat();
            let projected = project_smaps(&raw, &private).unwrap();
            assert_eq!(projected, guest);
            // Fallback is on the whole guest view, never the original input.
            assert_eq!(crate::procfs::sanitize_smaps(&projected, &table), guest);
        }
    }

    #[test]
    fn smaps_framing_and_mixed_accounting_refuse_before_identity_requests() {
        let private = [RuntimePrivateMemoryRegion {
            start: 0x3000,
            end: 0x4000,
        }];
        for raw in [
            b"Rss: 0 kB\n".as_slice(),
            b"3000-4000 ---p 0 00:00 0\n".as_slice(),
            b"3000-4000 ---p 0 00:00 0\nRss: 0 kB".as_slice(),
            b"3000-4000 ---p 0 00:00 0\nRss: 0 kB\n71000000-71001000: r-xp 0 00:00 9\n".as_slice(),
            b"3000-4000 ---p 0 00:00 0\nRss: 0 kB\nno colon\n".as_slice(),
            b"3000-4000 ---p 0 00:00 0\nRss: 0 kB\n1000-2000 r--p 0 00:00 0\nSize: 4 kB\n"
                .as_slice(),
            b"ffffffffffffffff-10000000000000000 r--p 0 00:00 0\nSize: 4 kB\n".as_slice(),
        ] {
            assert!(project_smaps(raw, &private).is_err(), "{raw:?}");
        }
        let mixed = b"2000-5000 r--p 0 08:01 7 /mixed\nSize: 12 kB\nRss: 4 kB\n";
        let mut requested = false;
        let result = project_smaps(mixed, &private).inspect(|_| {
            requested = true;
        });
        assert_eq!(
            result.unwrap_err(),
            "mixed ownership has unsupported per-VMA accounting"
        );
        assert!(!requested);
        assert!(
            project(
                mixed.split_inclusive(|b| *b == b'\n').next().unwrap(),
                &private
            )
            .is_ok(),
            "Maps retains its original mixed-row support"
        );
        let touching = [
            RuntimePrivateMemoryRegion {
                start: 0x2000,
                end: 0x3000,
            },
            RuntimePrivateMemoryRegion {
                start: 0x3000,
                end: 0x5000,
            },
        ];
        assert!(project_smaps(mixed, &touching).unwrap().is_empty());
        let adjacent = [RuntimePrivateMemoryRegion {
            start: 0x5000,
            end: 0x6000,
        }];
        assert_eq!(project_smaps(mixed, &adjacent).unwrap(), mixed);
        let overlapping = [
            RuntimePrivateMemoryRegion {
                start: 0x2000,
                end: 0x4000,
            },
            RuntimePrivateMemoryRegion {
                start: 0x3000,
                end: 0x5000,
            },
        ];
        assert!(project_smaps(mixed, &overlapping).is_err());
    }

    #[test]
    fn private_projection_binds_exact_maps_smaps_and_coherent_numa_leaves() {
        use crate::procfs::ProcfsFile;
        for (leaf, format) in [
            ("maps", Format::Maps),
            ("smaps", Format::Smaps),
            ("numa_maps", Format::NumaMaps),
        ] {
            let path = format!("/proc/self/{leaf}");
            let file = ProcfsFile::from_path(std::path::Path::new(&path)).unwrap();
            assert_eq!(file.private_mapping_format(), Some(format));
        }
        {
            let path = std::path::Path::new("/proc/self/smaps_rollup");
            let file = ProcfsFile::from_path(path).unwrap();
            assert_eq!(file.private_mapping_format(), None);
            assert_eq!(crate::procfs::maps_task_at(path, 11, 12), None);
        }
        for leaf in ["numa_map", "numa_maps/extra", "numa_maps_backup"] {
            let path = format!("/proc/self/{leaf}");
            assert_eq!(
                crate::procfs::maps_task_at(std::path::Path::new(&path), 11, 12),
                None
            );
        }
        assert_eq!(
            Format::NumaMaps.project(b"", &[]),
            Err("coherent companion Maps capture is required"),
            "NUMA cannot use an unpaired Maps/Smaps snapshot"
        );
    }
    struct KernelSmapsFixture {
        base: *mut libc::c_void,
        page: usize,
        len: usize,
        backing: std::fs::File,
    }

    impl KernelSmapsFixture {
        fn new() -> Self {
            use std::os::fd::AsRawFd;
            use std::os::fd::FromRawFd;
            let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                .expect("positive kernel page size");
            assert!(page.is_power_of_two());
            let len = page.checked_mul(7).expect("seven-page extent fits");
            let fd = unsafe {
                libc::memfd_create(c"private-smaps-kernel-control".as_ptr(), libc::MFD_CLOEXEC)
            };
            assert!(fd >= 0, "memfd_create: {}", std::io::Error::last_os_error());
            // A new memfd inode prevents this extent merging with unrelated VMAs.
            let backing = unsafe { std::fs::File::from_raw_fd(fd) };
            assert_eq!(
                unsafe { libc::ftruncate(backing.as_raw_fd(), len.try_into().unwrap()) },
                0,
                "ftruncate: {}",
                std::io::Error::last_os_error()
            );
            let base = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE,
                    backing.as_raw_fd(),
                    0,
                )
            };
            assert_ne!(
                base,
                libc::MAP_FAILED,
                "mmap: {}",
                std::io::Error::last_os_error()
            );
            Self {
                base,
                page,
                len,
                backing,
            }
        }

        fn boundary(&self, page: usize) -> u64 {
            assert!(page <= 7);
            (self.base as usize)
                .checked_add(self.page.checked_mul(page).unwrap())
                .unwrap()
                .try_into()
                .unwrap()
        }

        fn protect(&self, first: usize, pages: usize, prot: libc::c_int) {
            assert!(pages > 0 && first.checked_add(pages).is_some_and(|end| end <= 7));
            assert_eq!(
                unsafe {
                    libc::mprotect(
                        self.boundary(first) as usize as *mut libc::c_void,
                        pages * self.page,
                        prot,
                    )
                },
                0,
                "mprotect: {}",
                std::io::Error::last_os_error()
            );
        }

        fn capture(&self, smaps: bool) -> Vec<u8> {
            use std::os::unix::fs::MetadataExt;
            let raw = std::fs::read(if smaps {
                "/proc/self/smaps"
            } else {
                "/proc/self/maps"
            })
            .expect("read actual kernel snapshot");
            assert!(
                raw.ends_with(b"\n"),
                "complete newline-terminated kernel snapshot"
            );
            // Split at real headers; retain every byte of each selected section.
            // Never crop or rewrite a kernel row to fit the independently known extent.
            let mut starts = Vec::new();
            let mut offset = 0;
            for line in raw.split_inclusive(|byte| *byte == b'\n') {
                let body = &line[..line.len() - 1];
                if header(body).is_ok() {
                    starts.push(offset);
                } else {
                    assert!(
                        smaps && !starts.is_empty() && body.contains(&b':'),
                        "unexpected kernel mapping/field line: {body:?}"
                    );
                }
                offset += line.len();
            }
            starts.push(raw.len());
            let inode = self.backing.metadata().expect("memfd metadata").ino();
            let mut selected = Vec::new();
            let mut next = self.boundary(0);
            for pair in starts.windows(2) {
                let section = &raw[pair[0]..pair[1]];
                let line_end = section.iter().position(|byte| *byte == b'\n').unwrap();
                let row = header(&section[..line_end]).expect("kernel mapping header");
                if row.end <= self.boundary(0) || row.start >= self.boundary(7) {
                    continue;
                }
                assert!(
                    row.start >= self.boundary(0) && row.end <= self.boundary(7),
                    "a kernel VMA straddled the independently allocated extent"
                );
                assert_eq!(row.start, next, "exact ordered, gap-free extent coverage");
                assert_eq!(
                    row.inode, inode,
                    "the selected row belongs to our unique memfd"
                );
                assert_eq!(
                    row.offset,
                    row.start - self.boundary(0),
                    "real backing offset"
                );
                next = row.end;
                selected.extend_from_slice(section);
            }
            assert_eq!(next, self.boundary(7), "complete seven-page kernel extent");
            selected
        }
    }

    impl Drop for KernelSmapsFixture {
        fn drop(&mut self) {
            let result = unsafe { libc::munmap(self.base, self.len) };
            assert_eq!(result, 0, "release complete test extent");
        }
    }

    fn kernel_smaps_headers(bytes: &[u8]) -> Vec<(u64, u64, Vec<u8>)> {
        bytes
            .split_inclusive(|byte| *byte == b'\n')
            .filter_map(|line| {
                let body = line.strip_suffix(b"\n").unwrap();
                let row = header(body).ok()?;
                Some((row.start, row.end, body[row.fields[1].clone()].to_vec()))
            })
            .collect()
    }

    fn kernel_smaps_inode_requests(bytes: &[u8]) -> Vec<(u64, u64)> {
        std::str::from_utf8(bytes)
            .unwrap()
            .lines()
            .filter_map(super::super::mapping_header_identity)
            .collect()
    }

    #[test]
    fn real_private_commit_keeps_maps_and_smaps_guest_sections_and_inode_requests() {
        use super::super::BoundTask;
        use super::super::MapsTarget;
        use super::super::sanitize_maps;
        use super::super::sanitize_smaps;
        let fixture = KernelSmapsFixture::new();
        let task = BoundTask {
            process: 11,
            thread: None,
        };
        let mut answer = target_answer(task.process, fixture.boundary(2));
        let Some(reverie::RuntimePrivateMemory::Ready(private)) =
            answer.runtime_private_memory.as_mut()
        else {
            panic!("existing fixture provides ready ownership");
        };
        private.regions[0].end = fixture.boundary(5);
        let target = MapsTarget::from_open(task, &answer, &answer).unwrap();
        let regions = &target.private.regions;
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start, fixture.boundary(2));
        assert_eq!(regions[0].end, fixture.boundary(5));

        // Real initial VMA spans both guest and private portions: Smaps must not
        // fabricate per-fragment accounting, while existing Maps splitting survives.
        let mixed_maps = fixture.capture(false);
        let mixed_smaps = fixture.capture(true);
        let all_none = vec![(fixture.boundary(0), fixture.boundary(7), b"---p".to_vec())];
        assert_eq!(kernel_smaps_headers(&mixed_maps), all_none);
        assert_eq!(kernel_smaps_headers(&mixed_smaps), all_none);
        assert_eq!(
            project_smaps(&mixed_smaps, regions),
            Err("mixed ownership has unsupported per-VMA accounting"),
            "real mixed VMA must refuse for the declared accounting contract"
        );
        assert_eq!(
            kernel_smaps_headers(&project(&mixed_maps, regions).unwrap()).len(),
            2,
            "do not narrow existing Maps fragment support"
        );

        fixture.protect(0, 2, libc::PROT_READ);
        fixture.protect(5, 2, libc::PROT_READ);
        let before_maps = fixture.capture(false);
        let before_smaps = fixture.capture(true);
        let before_headers = vec![
            (fixture.boundary(0), fixture.boundary(2), b"r--p".to_vec()),
            (fixture.boundary(2), fixture.boundary(5), b"---p".to_vec()),
            (fixture.boundary(5), fixture.boundary(7), b"r--p".to_vec()),
        ];
        assert_eq!(kernel_smaps_headers(&before_maps), before_headers);
        assert_eq!(kernel_smaps_headers(&before_smaps), before_headers);

        // Commit the ENTIRE authoritative private interval, keeping exact private
        // boundaries distinct from the two read-only guest ranges.
        fixture.protect(2, 3, libc::PROT_READ | libc::PROT_WRITE);
        for page in 2..5 {
            unsafe { std::ptr::write_volatile(fixture.boundary(page) as usize as *mut u8, 1) };
        }
        let after_maps = fixture.capture(false);
        let after_smaps = fixture.capture(true);
        let mut after_headers = before_headers.clone();
        after_headers[1].2 = b"rw-p".to_vec();
        assert_eq!(kernel_smaps_headers(&after_maps), after_headers);
        assert_eq!(kernel_smaps_headers(&after_smaps), after_headers);
        assert_ne!(
            before_maps, after_maps,
            "actual kernel commitment is observable"
        );
        assert_ne!(
            before_smaps, after_smaps,
            "actual kernel Smaps input changed"
        );

        let projected_maps = project(&before_maps, regions).unwrap();
        let projected_smaps = project_smaps(&before_smaps, regions).unwrap();
        let requests = kernel_smaps_inode_requests(&projected_maps);
        assert_eq!(
            requests.len(),
            2,
            "one request per retained guest header, including duplicates"
        );
        assert_eq!(requests, kernel_smaps_inode_requests(&projected_smaps));
        let identities: std::collections::BTreeMap<_, _> = requests
            .iter()
            .copied()
            .map(|raw| (raw, (libc::makedev(0, 1), 77)))
            .collect();
        let guest_maps = sanitize_maps(&projected_maps, &identities);
        let guest_smaps = sanitize_smaps(&projected_smaps, &identities);
        let after_maps = project(&after_maps, regions).unwrap();
        let after_smaps = project_smaps(&after_smaps, regions).unwrap();
        assert_eq!(requests, kernel_smaps_inode_requests(&after_maps));
        assert_eq!(requests, kernel_smaps_inode_requests(&after_smaps));
        assert_eq!(guest_maps, sanitize_maps(&after_maps, &identities));
        assert_eq!(guest_smaps, sanitize_smaps(&after_smaps, &identities));

        // A real guest change must remain visible. READ -> NONE avoids merging a
        // guest row into the now-RW private VMA and testing a different contract.
        fixture.protect(0, 2, libc::PROT_NONE);
        let changed_maps = project(&fixture.capture(false), regions).unwrap();
        let changed_smaps = project_smaps(&fixture.capture(true), regions).unwrap();
        let expected_guest = vec![
            (fixture.boundary(0), fixture.boundary(2), b"---p".to_vec()),
            (fixture.boundary(5), fixture.boundary(7), b"r--p".to_vec()),
        ];
        assert_eq!(kernel_smaps_headers(&changed_maps), expected_guest);
        assert_eq!(kernel_smaps_headers(&changed_smaps), expected_guest);
        assert_ne!(guest_maps, sanitize_maps(&changed_maps, &identities));
        assert_ne!(guest_smaps, sanitize_smaps(&changed_smaps, &identities));
    }

    #[test]
    fn numa_private_splits_are_hidden_and_guest_splits_remain_visible() {
        let before_maps = b"1000-2000 r-xp 0 08:01 7 /guest\n3000-6000 ---p 0 00:00 0\n7000-8000 rw-p 0 08:01 9 /data\n";
        let after_maps = b"1000-2000 r-xp 0 08:01 7 /guest\n3000-4000 rw-p 0 00:00 0\n4000-5000 ---p 0 08:02 999 /private\n5000-6000 rw-p 0 00:00 0\n7000-8000 rw-p 0 08:01 9 /data\n";
        let before_numa = b"1000 default file=/guest mapped=1 N0=1\n3000 default\n7000 prefer:0 file=/data dirty=1 N0=1\n";
        let after_numa = b"1000 default file=/guest mapped=1 N0=1\n3000 default anon=1 N1=1\n4000 default future=value anon=0\n5000 default anon=1 N1=1\n7000 prefer:0 file=/data dirty=1 N0=1\n";
        let private = [RuntimePrivateMemoryRegion {
            start: 0x3000,
            end: 0x6000,
        }];
        let expected =
            b"1000 default file=/guest mapped=1 N0=1\n7000 prefer:0 file=/data dirty=1 N0=1\n";
        assert_eq!(
            project_numa(before_maps, before_numa, &private).unwrap(),
            expected
        );
        assert_eq!(
            project_numa(after_maps, after_numa, &private).unwrap(),
            expected
        );

        let guest_split_maps = b"1000-1800 r-xp 0 08:01 7 /guest\n1800-2000 r--p 800 08:01 7 /guest\n3000-6000 ---p 0 00:00 0\n7000-8000 rw-p 0 08:01 9 /data\n";
        let guest_split_numa = b"1000 default file=/guest\n1800 default file=/guest mapped=1 N0=1\n3000 default\n7000 prefer:0 file=/data dirty=1 N0=1\n";
        let changed = project_numa(guest_split_maps, guest_split_numa, &private).unwrap();
        assert_eq!(changed, b"1000 default file=/guest\n1800 default file=/guest mapped=1 N0=1\n7000 prefer:0 file=/data dirty=1 N0=1\n");
        assert_ne!(changed, expected);
    }

    #[test]
    fn numa_whole_guest_rows_preserve_raw_fields_and_sanitizer_fallback() {
        use super::super::sanitize_numa_maps;
        let maps = b"1000-2000 rw-p 0 08:01 7 /guest\n3000-4000 ---p 0 00:00 0\n";
        let private = [RuntimePrivateMemoryRegion {
            start: 0x3000,
            end: 0x4000,
        }];
        for guest in [
            &b"1000\tdefault  file=/guest future=value heap kernelpagesize_kB=4 anon=2 N0=2\n"[..],
            &b"1000 default anon=bad N0=2\n"[..],
            &b"1000 default file=/guest-\xff anon=2\n"[..],
        ] {
            let mut raw = guest.to_vec();
            raw.extend_from_slice(b"3000 default anon=1 future=value\n");
            let projected = project_numa(maps, &raw, &private).unwrap();
            assert_eq!(
                projected, guest,
                "projection must not sanitize whole guest rows"
            );
            assert_eq!(sanitize_numa_maps(&projected), sanitize_numa_maps(guest));
        }
        // Maps may contain entries absent from NUMA. A missing exact match for
        // a NUMA row is an error; a missing NUMA row is not an inferred extent.
        assert_eq!(
            project_numa(maps, b"1000 default\n", &[]).unwrap(),
            b"1000 default\n"
        );
    }

    #[test]
    fn numa_mixed_prefix_preserves_start_policy_and_file_bytes() {
        let maps = b"1000-5000 rw-s 2000 00:01 7 /dev/shm/data\n7000-8000 rw-p 0 00:00 0\n";
        let private = [
            RuntimePrivateMemoryRegion {
                start: 0x3000,
                end: 0x4000,
            },
            RuntimePrivateMemoryRegion {
                start: 0x4000,
                end: 0x5000,
            },
        ];
        for prefix in [
            &b"1000 default\n"[..],
            &b"1000\tbind:0-1  file=/dev/shm/data\\040name\n"[..],
            &b"1000 prefer (many)=static|balancing:0-1 file=/dev/shm/data\n"[..],
            &b"1000 weighted interleave=relative:0-2\n"[..],
        ] {
            // An unrelated whole guest row can invoke the sanitizer's fallback;
            // it does not authorize weakening the mixed row's field restriction.
            let mut raw = prefix.to_vec();
            raw.extend_from_slice(b"7000 default file=/guest-\xff anon=bad\n");
            assert_eq!(project_numa(maps, &raw, &private).unwrap(), raw);
        }
    }

    #[test]
    fn numa_changed_start_or_multiple_fragments_refuse_even_without_counters() {
        let maps = b"1000-5000 rw-s 2000 00:01 7 /dev/shm/data\n";
        let numa = b"1000 bind:0 file=/dev/shm/data\n";
        for private in [
            RuntimePrivateMemoryRegion {
                start: 0x1000,
                end: 0x2000,
            },
            RuntimePrivateMemoryRegion {
                start: 0x2000,
                end: 0x3000,
            },
        ] {
            assert_eq!(
                project_numa(maps, numa, &[private]),
                Err("mixed numa mapping changes policy lookup start"),
                "a start inside a private interval cannot hide a guest suffix"
            );
        }
    }

    #[test]
    fn numa_mixed_fields_have_specific_refusal_reasons() {
        let maps = b"1000-5000 rw-p 0 00:00 0\n";
        let private = [RuntimePrivateMemoryRegion {
            start: 0x3000,
            end: 0x5000,
        }];
        for (field, reason) in [
            ("heap", "mixed numa mapping has region tags"),
            ("stack", "mixed numa mapping has region tags"),
            ("huge", "mixed numa mapping has region tags"),
            (
                "kernelpagesize_kB=4",
                "mixed numa mapping has page-size presence",
            ),
            ("future=1", "mixed numa mapping has unknown fields"),
            ("N=1", "mixed numa mapping has unknown fields"),
            ("Nother=1", "mixed numa mapping has unknown fields"),
            ("file=", "mixed numa mapping has unsupported file fields"),
            (
                "file=/a file=/b",
                "mixed numa mapping has unsupported file fields",
            ),
        ] {
            let numa = format!("1000 default {field}\n");
            assert_eq!(project_numa(maps, numa.as_bytes(), &private), Err(reason));
        }
        for name in [
            "active",
            "anon",
            "dirty",
            "mapped",
            "mapmax",
            "swapcache",
            "writeback",
            "N0",
            "N12",
        ] {
            for value in ["0", "1", "18446744073709551615"] {
                let numa = format!("1000 default {name}={value}\n");
                assert_eq!(
                    project_numa(maps, numa.as_bytes(), &private),
                    Err("mixed numa mapping has residency fields")
                );
            }
            for value in ["", "bad", "-1", "18446744073709551616"] {
                let numa = format!("1000 default {name}={value}\n");
                assert_eq!(
                    project_numa(maps, numa.as_bytes(), &private),
                    Err("malformed numa accounting fallback")
                );
            }
        }
        assert_eq!(
            project_numa(maps, b"1000 default file=/a-\xff\n", &private),
            Err("non-UTF8 numa sanitizer fallback")
        );
        for policy in ["weighted", "weighted unknown", "default unexpected"] {
            let numa = format!("1000 {policy}\n");
            assert!(project_numa(maps, numa.as_bytes(), &private).is_err());
        }
    }

    #[test]
    fn numa_validates_row_and_maps_framing_before_private_filtering() {
        let maps = b"1000-2000 rw-p 0 00:00 0\n3000-4000 rw-p 0 00:00 0\n";
        let private = [RuntimePrivateMemoryRegion {
            start: 0x1000,
            end: 0x5000,
        }];
        for (numa, reason) in [
            (&b"1000 default"[..], "truncated numa snapshot"),
            (&b"\n"[..], "missing numa start address"),
            (&b"x default\n"[..], "invalid numa start address"),
            (
                &b"10000000000000000 default\n"[..],
                "invalid numa start address",
            ),
            (&b"1000\n"[..], "missing numa policy"),
            (
                &b"1000 default\n1000 default\n"[..],
                "duplicate or unordered numa rows",
            ),
            (
                &b"3000 default\n1000 default\n"[..],
                "duplicate or unordered numa rows",
            ),
            (&b"1800 default\n"[..], "numa start has no exact maps match"),
            (&b"2000 default\n"[..], "numa start has no exact maps match"),
            (&b"5000 default\n"[..], "numa start has no exact maps match"),
            (
                &b"1000 default anon=bad\n"[..],
                "malformed numa accounting fallback",
            ),
            (
                &b"1000 default N0=18446744073709551616\n"[..],
                "malformed numa accounting fallback",
            ),
            (
                &b"1000 default file=/private-\xff\n"[..],
                "non-UTF8 numa sanitizer fallback",
            ),
        ] {
            assert_eq!(project_numa(maps, numa, &private), Err(reason));
        }
        for bad_maps in [
            &b"1000-2000 rw-p 0 00:00 0"[..],
            &b"2000-1000 rw-p 0 00:00 0\n"[..],
            &b"1000-2000 rw-p 0 00:00 nope\n"[..],
            &b"1000-2000 rw-p 10000000000000000 00:00 0\n"[..],
            &b"1000-3000 rw-p 0 00:00 0\n2000-4000 rw-p 0 00:00 0\n"[..],
            &b"3000-4000 rw-p 0 00:00 0\n1000-2000 rw-p 0 00:00 0\n"[..],
        ] {
            assert!(project_numa(bad_maps, b"1000 default\n", &private).is_err());
        }
        for regions in [
            vec![RuntimePrivateMemoryRegion { start: 2, end: 1 }],
            vec![
                RuntimePrivateMemoryRegion { start: 1, end: 3 },
                RuntimePrivateMemoryRegion { start: 2, end: 4 },
            ],
        ] {
            assert_eq!(
                project_numa(maps, b"1000 default\n", &regions),
                Err("invalid runtime-private interval union")
            );
        }
        // Otherwise dropping the second row would suppress the sanitizer's
        // whole-view fallback and strip counters from the first guest row.
        assert_eq!(
            project_numa(
                maps,
                b"1000 default anon=1 N0=1\n3000 default dirty=invalid\n",
                &[RuntimePrivateMemoryRegion {
                    start: 0x3000,
                    end: 0x4000,
                }],
            ),
            Err("malformed numa accounting fallback")
        );
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
