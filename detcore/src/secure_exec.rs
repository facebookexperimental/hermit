/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Whether Linux runs an exec in secure-execution mode for the guest, decided
//! the same way on every backend from the new image's set-id bits and
//! Detcore's fixed virtual-root identity, and what Detcore does with that
//! decision once the new image reports its own `AT_SECURE`.
//!
//! Linux decides it in `bprm_fill_uid` and `cap_bprm_creds_from_file`
//! (fs/exec.c and security/commoncap.c, v6.17): a set-user-ID bit makes the
//! file's owner the effective user id, a set-group-ID bit with group execute
//! makes the file's group the effective group id, unless the mount is
//! `nosuid`, the task has `no_new_privs`, or the user namespace maps neither
//! the owner nor the group; and the exec is secure when that moves the
//! effective user or group id away from the caller's real one, which for
//! Detcore's virtual root is 0. A secure exec clears the parent-death signal
//! (`begin_new_exec`) and reports `AT_SECURE` 1 (`create_elf_tables`).

use std::path::Path;

/// The facts about an image file that decide its set-id transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SetIdImage {
    /// Its `st_mode`.
    pub mode: u32,
    /// Its owner and group, as the guest's user namespace shows them.
    pub uid: u32,
    pub gid: u32,
    /// Whether that namespace maps the owner and the group.
    pub uid_mapped: bool,
    pub gid_mapped: bool,
    /// Whether its mount is `nosuid`.
    pub nosuid: bool,
}

impl SetIdImage {
    /// Whether the file has a set-user-ID or set-group-ID bit at all, the
    /// first test of `bprm_fill_uid`.
    pub fn has_set_id_bits(&self) -> bool {
        self.mode & (libc::S_ISUID | libc::S_ISGID) != 0
    }
}

/// The effective user and group ids Linux gives a caller whose real and
/// effective user and group ids are all 0 (Detcore's fixed virtual root) at
/// an exec of `image`, with `no_new_privs` as the task has it
/// (`bprm_fill_uid`, fs/exec.c:1534-1583, v6.17): a set-user-ID bit makes the
/// file's owner the effective user id, and a set-group-ID bit with group
/// execute makes the file's group the effective group id, unless the task
/// has `no_new_privs`, the mount is `nosuid`, or the namespace maps the
/// owner or the group not at all. The filesystem ids follow
/// (`cap_bprm_creds_from_file`). Anything but (0, 0) moves an effective id
/// away from the real one, 0, which is what makes the exec run in
/// secure-execution mode (security/commoncap.c:989-996). File capabilities
/// never make such an exec secure: root is already fully capable in its
/// namespace (`handle_privileged_root`).
pub(crate) fn effective_ids_for_virtual_root(image: &SetIdImage, no_new_privs: bool) -> (u32, u32) {
    if no_new_privs || image.nosuid || !image.has_set_id_bits() {
        return (0, 0);
    }
    // An owner or group the namespace does not map voids both bits.
    if !image.uid_mapped || !image.gid_mapped {
        return (0, 0);
    }
    let setuid = image.mode & libc::S_ISUID != 0;
    let setgid = image.mode & (libc::S_ISGID | libc::S_IXGRP) == (libc::S_ISGID | libc::S_IXGRP);
    (
        if setuid { image.uid } else { 0 },
        if setgid { image.gid } else { 0 },
    )
}

/// The set-id facts of the image file at `path`, as the tool sees it: the
/// tool shares the guest's user and mount namespaces wherever Detcore
/// resolves the image itself (`Detcore::exec_record_root`). `None` where the
/// file or its mount cannot be read. Raw syscalls: in-guest Detcore must not
/// call libc's wrappers (`crate::util::raw_syscall`).
pub(crate) fn set_id_image(path: &Path) -> Option<SetIdImage> {
    let stat = crate::util::raw_stat(path).ok()?;
    let nosuid = mount_flags(path)? & libc::ST_NOSUID != 0;
    let uid_map = crate::util::raw_read_file(Path::new("/proc/self/uid_map")).ok()?;
    let gid_map = crate::util::raw_read_file(Path::new("/proc/self/gid_map")).ok()?;
    Some(SetIdImage {
        mode: stat.st_mode,
        uid: stat.st_uid,
        gid: stat.st_gid,
        uid_mapped: maps(&uid_map, stat.st_uid),
        gid_mapped: maps(&gid_map, stat.st_gid),
        nosuid,
    })
}

/// The mount flags (`statfs::f_flags`) of the file system holding `path`, by
/// a raw `statfs`. libc's `statfs` keeps `f_flags` private, so the kernel's
/// structure is read as the fifteen words it is on x86_64 and aarch64, where
/// `f_flags` is the eleventh (include/uapi/asm-generic/statfs.h).
fn mount_flags(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    const WORDS: usize = 15;
    const FLAGS: usize = 10;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut words = [0_u64; WORDS];
    // SAFETY: the path is NUL-terminated and `words` is writable and as
    // large as the kernel's structure.
    crate::util::raw_zero_result(unsafe {
        crate::util::raw_syscall(
            libc::SYS_statfs,
            [path.as_ptr() as u64, words.as_mut_ptr() as u64, 0, 0, 0, 0],
        )
    })
    .ok()?;
    Some(words[FLAGS])
}

/// Whether the id map `map` (the contents of a `uid_map` or `gid_map`) maps
/// `id` inside its namespace: some `inside outside count` line covers it.
pub(crate) fn maps(map: &[u8], id: u32) -> bool {
    String::from_utf8_lossy(map).lines().any(|line| {
        let fields: Vec<u64> = line
            .split_whitespace()
            .filter_map(|field| field.parse().ok())
            .collect();
        matches!(fields.as_slice(), [inside, _, count]
            if (*inside..inside.saturating_add(*count)).contains(&u64::from(id)))
    })
}

/// The `NoNewPrivs:` value of a `/proc/<pid>/status` text, `None` where the
/// line is missing or malformed.
pub(crate) fn no_new_privs(status: &[u8]) -> Option<bool> {
    String::from_utf8_lossy(status)
        .lines()
        .find_map(|line| line.strip_prefix("NoNewPrivs:"))
        .and_then(|value| match value.trim() {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        })
}

/// What Detcore does at a new image's first callback, from whether Linux
/// runs the exec securely (`expected`, decided before the exec; `None` where
/// it was not decided) and the image's own `AT_SECURE` (`reported`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecureExecOutcome {
    /// An ordinary exec: nothing to do.
    Ordinary,
    /// The exec ran in secure-execution mode, as the image reports: clear
    /// the parent-death signal Detcore holds, as Linux's `begin_new_exec`
    /// clears the task's.
    Secure,
    /// Linux runs the exec securely, but the backend ran it without
    /// secure-execution mode (`AT_SECURE` 0): the new image's credentials,
    /// auxiliary vector and dumpability differ from Linux's, so the run is
    /// refused by name. The parent-death signal is cleared as for `Secure`.
    Unsupported,
}

/// [`SecureExecOutcome`] for `expected` and `reported`. A backend that ran
/// the exec securely is believed even where Detcore did not expect it: the
/// kernel that performed it decided with the task's real credentials.
pub(crate) fn outcome(expected: Option<bool>, reported: Option<bool>) -> SecureExecOutcome {
    match (expected, reported) {
        (_, Some(true)) => SecureExecOutcome::Secure,
        (Some(true), _) => SecureExecOutcome::Unsupported,
        _ => SecureExecOutcome::Ordinary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(mode: u32, uid: u32, gid: u32) -> SetIdImage {
        SetIdImage {
            mode: libc::S_IFREG | mode,
            uid,
            gid,
            uid_mapped: true,
            gid_mapped: true,
            nosuid: false,
        }
    }

    /// A set-user-ID image owned by someone else, or a set-group-ID image
    /// with group execute whose group is not 0, gives the virtual root that
    /// owner or group as its effective id and makes the exec secure; an image
    /// owned by root, a set-group-ID bit without group execute, and plain
    /// images change neither.
    #[test]
    fn set_id_bits_that_move_an_effective_id_make_the_exec_secure() {
        for (mode, uid, gid, ids) in [
            (0o4755, 1000, 0, (1000, 0)),
            (0o4755, 0, 0, (0, 0)),
            (0o2755, 0, 1000, (0, 1000)),
            (0o2745, 0, 1000, (0, 0)),
            (0o2755, 0, 0, (0, 0)),
            (0o6755, 0, 1000, (0, 1000)),
            (0o6755, 1000, 1000, (1000, 1000)),
            (0o0755, 1000, 1000, (0, 0)),
        ] {
            assert_eq!(
                effective_ids_for_virtual_root(&image(mode, uid, gid), false),
                ids,
                "{mode:o} {uid} {gid}"
            );
        }
    }

    /// `no_new_privs`, a `nosuid` mount and an owner or group the namespace
    /// does not map each void the set-id bits, as `bprm_fill_uid` does.
    #[test]
    fn no_new_privs_nosuid_and_unmapped_ids_void_the_set_id_bits() {
        let secure = image(0o4755, 1000, 0);
        assert_eq!(effective_ids_for_virtual_root(&secure, false), (1000, 0));
        assert_eq!(effective_ids_for_virtual_root(&secure, true), (0, 0));
        for void in [
            SetIdImage {
                nosuid: true,
                ..secure
            },
            SetIdImage {
                uid_mapped: false,
                ..secure
            },
            SetIdImage {
                gid_mapped: false,
                ..secure
            },
        ] {
            assert_eq!(
                effective_ids_for_virtual_root(&void, false),
                (0, 0),
                "{void:?}"
            );
        }
    }

    /// An id map covers exactly its ranges; a status text yields its
    /// `NoNewPrivs:` value.
    #[test]
    fn id_maps_and_no_new_privs_are_read_as_linux_writes_them() {
        let single = b"         0     212630          1\n";
        assert!(maps(single, 0));
        assert!(!maps(single, 1));
        assert!(!maps(single, 65534));
        let wide = b"         0     212630          1\n         1 1879048192      65536\n";
        assert!(maps(wide, 1000));
        assert!(maps(wide, 65536));
        assert!(!maps(wide, 65537));
        assert!(maps(b"         0          0 4294967295\n", 65534));
        assert_eq!(no_new_privs(b"Name:\tx\nNoNewPrivs:\t1\n"), Some(true));
        assert_eq!(no_new_privs(b"NoNewPrivs:\t0\n"), Some(false));
        assert_eq!(no_new_privs(b"Name:\tx\n"), None);
    }

    /// The image's own `AT_SECURE` decides a secure exec, expected or not; an
    /// exec Linux runs securely that the backend ran without it is
    /// unsupported; anything else is ordinary.
    #[test]
    fn the_outcome_follows_the_image_and_refuses_a_missing_secure_exec() {
        use SecureExecOutcome::*;
        for (expected, reported, result) in [
            (None, None, Ordinary),
            (None, Some(false), Ordinary),
            (Some(false), Some(false), Ordinary),
            (None, Some(true), Secure),
            (Some(false), Some(true), Secure),
            (Some(true), Some(true), Secure),
            (Some(true), Some(false), Unsupported),
            (Some(true), None, Unsupported),
        ] {
            assert_eq!(
                outcome(expected, reported),
                result,
                "{expected:?} {reported:?}"
            );
        }
    }
}
