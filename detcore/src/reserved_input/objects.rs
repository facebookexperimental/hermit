/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Classification of actual, pinned native descriptions. Guest/model stat
//! answers and pathname strings alone never qualify a control object.

use reverie::reserved_input::InputAdmission;
use reverie::reserved_input::InputDescriptorFacts;
use reverie::reserved_input::InputImagePhase;
use reverie::reserved_input::InputNativeFacts;
use reverie::reserved_input::InputUnavailable;

const PROC_SUPER_MAGIC: u64 = 0x9fa0;
const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

pub(crate) fn admit_initial(facts: &InputNativeFacts) -> InputAdmission {
    let unqualified = |reason| InputAdmission::Unqualified(InputUnavailable::new(reason));
    for limit in [&facts.cpu, &facts.realtime] {
        match limit {
            Ok(limit) if limit.current == u64::MAX && limit.maximum == u64::MAX => {}
            Ok(_) => return unqualified("physical-cpu-or-rttime-limit-finite"),
            Err(_) => return unqualified("physical-cpu-or-rttime-limit-unverified"),
        }
    }
    // The owned launcher is not the target image. Its libc registration is
    // reset by successful exec; raw function launch has no such boundary.
    if facts.phase == InputImagePhase::Target {
        match &facts.rseq {
            Ok(rseq) if rseq.address == 0 => {}
            Ok(_) => return unqualified("native-rseq-reference-unverified"),
            Err(_) => return unqualified("native-rseq-initial-state-unverified"),
        }
    }
    let descriptors = match &facts.descriptors {
        Ok(descriptors) => descriptors,
        Err(_) => return unqualified("initial-descriptor-census-unverified"),
    };
    for descriptor in descriptors {
        if let Some(cause) = descriptor_cause(descriptor.facts(), facts.mountinfo.as_deref().ok()) {
            return unqualified(cause);
        }
    }
    InputAdmission::Qualified
}

/// Return a named producer or missing-proof cause. This does not change the
/// native operation, owner, flags, limits or descriptor, and does not assume
/// that sampling excludes mutation through an external alias.
pub(crate) fn descriptor_cause(
    descriptor: &InputDescriptorFacts,
    mountinfo: Option<&[u8]>,
) -> Option<&'static str> {
    let flags = match descriptor.status_flags {
        Ok(flags) => flags,
        Err(_) => return Some("descriptor-native-status-unverified"),
    };
    if flags & libc::O_PATH != 0 {
        // O_PATH cannot be upgraded by F_SETFL into a writable/fasync file.
        // Reopening it is a separate descriptor acquisition.
        return None;
    }
    match descriptor.async_signal {
        Ok(libc::SIGKILL) => return Some("inherited-fasync-sigkill-producer"),
        Ok(_) => {}
        Err(_) => return Some("descriptor-native-fasync-signal-unverified"),
    }
    if descriptor.async_owner.is_err() {
        return Some("descriptor-native-fasync-owner-unverified");
    }
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return None;
    }
    let (_, _, mode) = match descriptor.identity {
        Ok(identity) => identity,
        Err(_) => return Some("descriptor-native-identity-unverified"),
    };
    if mode & libc::S_IFMT != libc::S_IFREG {
        return None;
    }
    let filesystem = match descriptor.filesystem {
        Ok(filesystem) => filesystem,
        Err(_) => return Some("descriptor-native-filesystem-unverified"),
    };
    if !matches!(filesystem, PROC_SUPER_MAGIC | CGROUP2_SUPER_MAGIC) {
        return None;
    }
    let Some(path) = mountinfo.and_then(|mountinfo| internal_path(descriptor, mountinfo)) else {
        return Some("descriptor-control-object-unverified");
    };
    if filesystem == PROC_SUPER_MAGIC && path == b"/sysrq-trigger" {
        Some("writable-sysrq-trigger-terminal-producer")
    } else if filesystem == CGROUP2_SUPER_MAGIC
        && path.rsplit(|byte| *byte == b'/').next() == Some(b"cgroup.kill".as_slice())
    {
        Some("writable-cgroup-kill-terminal-producer")
    } else {
        None
    }
}

/// Reconstruct the kernel filesystem-internal path, including a bind mount
/// whose root is the control file itself. Empty suffix is a real match.
pub(super) fn internal_path(
    descriptor: &InputDescriptorFacts,
    mountinfo: &[u8],
) -> Option<Vec<u8>> {
    if !descriptor.path_context.as_ref().ok()?.same_view() {
        return None;
    }
    let mount_id = descriptor.mount_id.as_ref().ok()?;
    let (device, _, _) = descriptor.identity.as_ref().ok()?;
    let path = descriptor.proc_path.as_ref().ok()?;
    if !path.starts_with(b"/") || path.ends_with(b" (deleted)") {
        return None;
    }
    let rows = crate::procfs::parse_mountinfo(mountinfo)?;
    let row = rows.iter().find(|row| row.raw_mount_id == *mount_id)?;
    if row.raw_device != *device {
        return None;
    }
    let line = mountinfo.split(|byte| *byte == b'\n').find(|line| {
        line.split(|byte| *byte == b' ')
            .next()
            .and_then(|id| std::str::from_utf8(id).ok())
            .and_then(|id| id.parse::<u64>().ok())
            == Some(*mount_id)
    })?;
    let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
    let root = unescape(fields.get(3)?)?;
    let mountpoint = unescape(fields.get(4)?)?;
    if !root.starts_with(b"/") || !mountpoint.starts_with(b"/") {
        return None;
    }
    let suffix = if mountpoint == b"/" {
        path.as_slice()
    } else {
        let suffix = path.strip_prefix(mountpoint.as_slice())?;
        if !suffix.is_empty() && !suffix.starts_with(b"/") {
            return None;
        }
        suffix
    };
    let mut internal = root;
    if internal.ends_with(b"/") && suffix.starts_with(b"/") {
        internal.pop();
    }
    internal.extend_from_slice(suffix);
    Some(internal)
}

fn unescape(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let digits = bytes.get(index + 1..index + 4)?;
            if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                return None;
            }
            let value = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            output.push(u8::try_from(value).ok()?);
            index += 4;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use reverie::reserved_input::InputNativeError;
    use reverie::reserved_input::InputPathContext;

    use super::*;

    fn descriptor(filesystem: u64, path: &[u8]) -> InputDescriptorFacts {
        InputDescriptorFacts {
            fd: 300,
            status_flags: Ok(libc::O_WRONLY),
            async_signal: Ok(0),
            async_owner: Ok((crate::host_timed_producers::F_OWNER_PID, 0)),
            identity: Ok((libc::makedev(0, 23), 91, libc::S_IFREG | 0o200)),
            filesystem: Ok(filesystem),
            mount_id: Ok(27),
            proc_path: Ok(path.to_vec()),
            path_context: Ok(InputPathContext {
                reader_mount_namespace: (1, 2),
                target_mount_namespace: (1, 2),
                reader_root: (3, 4),
                target_root: (3, 4),
                reader_cgroup_namespace: (5, 6),
                target_cgroup_namespace: (5, 6),
            }),
        }
    }

    #[test]
    fn native_control_objects_include_bind_alias_and_empty_suffix() {
        let mount = b"27 1 0:23 /sysrq-trigger /ordinary-file rw - proc proc rw\n";
        let mut fd = descriptor(PROC_SUPER_MAGIC, b"/ordinary-file");
        assert_eq!(
            descriptor_cause(&fd, Some(mount)),
            Some("writable-sysrq-trigger-terminal-producer")
        );
        fd.status_flags = Ok(libc::O_RDONLY);
        assert_eq!(descriptor_cause(&fd, Some(mount)), None);
        fd.status_flags = Ok(libc::O_PATH);
        fd.async_signal = Err(InputNativeError {
            operation: "F_GETSIG",
            errno: Some(libc::EBADF),
        });
        assert_eq!(descriptor_cause(&fd, Some(mount)), None);
        let ordinary = descriptor(0xef53, b"/sysrq-trigger");
        assert_eq!(descriptor_cause(&ordinary, None), None);
        let cgroup = descriptor(CGROUP2_SUPER_MAGIC, b"/bound/name");
        let mount = b"27 1 0:23 /subgroup/cgroup.kill /bound/name rw - cgroup2 cgroup rw\n";
        assert_eq!(
            descriptor_cause(&cgroup, Some(mount)),
            Some("writable-cgroup-kill-terminal-producer")
        );
    }

    #[test]
    fn fasync_and_unknown_path_facts_are_not_owner_or_filename_exemptions() {
        let mut fd = descriptor(0xef53, b"/ordinary");
        fd.status_flags = Ok(libc::O_RDONLY); // neither O_ASYNC nor a nonzero owner
        fd.async_signal = Ok(libc::SIGKILL);
        assert_eq!(
            descriptor_cause(&fd, None),
            Some("inherited-fasync-sigkill-producer")
        );
        let mut fd = descriptor(PROC_SUPER_MAGIC, b"/proc/not-a-control");
        fd.path_context.as_mut().unwrap().target_root = (9, 10);
        assert_eq!(
            descriptor_cause(&fd, Some(b"27 1 0:23 / /proc rw - proc proc rw\n")),
            Some("descriptor-control-object-unverified")
        );
    }
}
