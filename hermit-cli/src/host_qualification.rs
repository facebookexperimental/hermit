/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Expensive one-time host qualifications, cached in the user's cache
//! directory (`$XDG_CACHE_HOME/hermit/`, by default `~/.cache/hermit/`), so
//! that a later run reuses the verdict instead of measuring the host again
//! (<https://github.com/rrnewton/hermit/issues/4515>).
//!
//! Each qualification is one file holding one verdict and the key it was
//! measured under: the kernel release, the boot, the CPU (vendor, family,
//! model, stepping, microcode), the Hermit binary that measured it, this
//! cache's schema and the qualification's own inputs. A reboot, a kernel or
//! CPU change, a microcode update, a rebuilt or reinstalled Hermit, or new
//! inputs miss and measure again. Only verdicts that depend on the host alone
//! belong here: nothing in the key can tell a chroot or container from the
//! host, so a check whose answer depends on the caller's sandbox (namespaces,
//! ptrace, seccomp) must run every time instead. Only a passing verdict is
//! stored. The cache is best effort: a missing, unreadable, oversized,
//! non-regular, corrupt or unwritable file is a miss, never a failure.

use std::fs;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Bumped whenever what a cached verdict means changes, which discards every
/// older file.
const SCHEMA: u32 = 1;

/// The largest cache file read; a larger one is a miss.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// The host facts a cached verdict holds for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HostKey {
    pub(crate) kernel_release: String,
    pub(crate) boot_id: String,
    pub(crate) cpu: String,
    pub(crate) microcode: String,
    /// The running Hermit binary's device, inode, size and modification
    /// time: a qualification's own code is part of what it certifies, so a
    /// verdict measured by another build is never reused.
    pub(crate) hermit_binary: String,
}

fn read_trimmed(path: &str) -> String {
    fs::read_to_string(path)
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

impl HostKey {
    /// This host's key: `uname`, the boot id, CPUID and the microcode
    /// revision, a few constant-time reads.
    pub(crate) fn current() -> Self {
        Self {
            kernel_release: nix::sys::utsname::uname()
                .map(|uts| uts.release().to_string_lossy().into_owned())
                .unwrap_or_default(),
            boot_id: read_trimmed("/proc/sys/kernel/random/boot_id"),
            cpu: cpu_signature(),
            microcode: read_trimmed("/sys/devices/system/cpu/cpu0/microcode/version"),
            hermit_binary: binary_identity(),
        }
    }
}

/// `/proc/self/exe`'s device, inode, size and modification time.
fn binary_identity() -> String {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self/exe")
        .map(|m| {
            format!(
                "{}:{}:{}:{}.{}",
                m.dev(),
                m.ino(),
                m.size(),
                m.mtime(),
                m.mtime_nsec()
            )
        })
        .unwrap_or_default()
}

/// The CPU's vendor, family, model and stepping, from CPUID leaves 0 and 1.
#[cfg(target_arch = "x86_64")]
fn cpu_signature() -> String {
    // CPUID leaves 0 and 1 exist on every x86_64 CPU.
    let (leaf0, leaf1) = (std::arch::x86_64::__cpuid(0), std::arch::x86_64::__cpuid(1));
    signature_of([leaf0.ebx, leaf0.edx, leaf0.ecx], leaf1.eax)
}

/// The vendor, family, model and stepping that CPUID leaf 0's vendor words
/// and leaf 1's EAX name, as Linux's `/proc/cpuinfo` decodes them.
fn signature_of(vendor_words: [u32; 3], eax: u32) -> String {
    let vendor: Vec<u8> = vendor_words
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect();
    let base_family = (eax >> 8) & 0xf;
    let family = if base_family == 0xf {
        base_family + ((eax >> 20) & 0xff)
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        ((eax >> 12) & 0xf0) | ((eax >> 4) & 0xf)
    } else {
        (eax >> 4) & 0xf
    };
    format!(
        "{} family {family:#x} model {model:#x} stepping {:#x}",
        String::from_utf8_lossy(&vendor),
        eax & 0xf
    )
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_signature() -> String {
    String::new()
}

/// One qualification's file.
#[derive(Debug, Serialize, Deserialize)]
struct Entry<T> {
    schema: u32,
    host: HostKey,
    /// The qualification's own inputs, as it describes them.
    inputs: String,
    verdict: T,
}

/// Where a qualification named `name` is cached, or `None` without a home
/// directory.
pub(crate) fn default_path(name: &str) -> Option<PathBuf> {
    dirs::cache_dir().map(|dir| dir.join("hermit").join(format!("{name}.json")))
}

/// The verdict cached at `path` for `host` and `inputs`, if there is one.
pub(crate) fn load<T: DeserializeOwned>(path: &Path, host: &HostKey, inputs: &str) -> Option<T> {
    let bytes = read_bounded(path)?;
    let entry: Entry<T> = serde_json::from_slice(&bytes).ok()?;
    (entry.schema == SCHEMA && entry.host == *host && entry.inputs == inputs)
        .then_some(entry.verdict)
}

/// Caches `verdict` at `path` for `host` and `inputs`, by an atomic rename of
/// a file in the same directory, and says whether it did. A failure leaves
/// the cache as it was.
pub(crate) fn store<T: Serialize>(path: &Path, host: &HostKey, inputs: &str, verdict: &T) -> bool {
    let entry = Entry {
        schema: SCHEMA,
        host: host.clone(),
        inputs: inputs.to_owned(),
        verdict,
    };
    write_atomically(path, &entry).is_ok()
}

/// Whether a verdict could be stored at `path`: its directory exists, or can
/// be made, and this process may write it.
pub(crate) fn writable(path: &Path) -> bool {
    path.parent().is_some_and(|directory| {
        fs::create_dir_all(directory).is_ok()
            && nix::unistd::access(directory, nix::unistd::AccessFlags::W_OK).is_ok()
    })
}

/// Runs `f` holding an exclusive `flock` on `<path>.lock`, so that processes
/// that miss at once measure one at a time and the later ones can find the
/// first one's verdict. Without a lock file `f` runs unlocked.
pub(crate) fn locked<R>(path: &Path, f: impl FnOnce() -> R) -> R {
    use std::os::fd::AsRawFd;
    let lock = path.parent().and_then(|directory| {
        fs::create_dir_all(directory).ok()?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path.with_extension("lock"))
            .ok()?;
        // SAFETY: the descriptor is open for the duration of the call.
        (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0).then_some(file)
    });
    let result = f();
    drop(lock);
    result
}

/// Reads `path` if it is a regular file of at most [`MAX_FILE_BYTES`] that
/// this process's user owns. The open neither blocks on a FIFO nor follows a
/// symbolic link.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    read_bounded_as(path, nix::unistd::geteuid().as_raw())
}

/// [`read_bounded`] for a file that user `owner` must own.
fn read_bounded_as(path: &Path, owner: u32) -> Option<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES || metadata.uid() != owner {
        return None;
    }
    // A file that grows after the check is cut at the cap, so it does not
    // parse and is a miss.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

fn write_atomically<T: Serialize>(path: &Path, entry: &T) -> std::io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the cache path has no directory"))?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&serde_json::to_vec_pretty(entry).map_err(std::io::Error::other)?)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostKey {
        HostKey {
            kernel_release: "7.1.3".into(),
            boot_id: "boot-a".into(),
            cpu: "AuthenticAMD family 0x1a model 0x11 stepping 0x0".into(),
            microcode: "0xb101054".into(),
            hermit_binary: "64768:123:456:789.0".into(),
        }
    }

    #[test]
    fn a_stored_verdict_is_loaded_for_the_same_host_and_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hermit").join("v.json");
        assert_eq!(load::<u32>(&path, &host(), "in"), None);
        store(&path, &host(), "in", &7u32);
        assert_eq!(load::<u32>(&path, &host(), "in"), Some(7));
    }

    #[test]
    fn a_change_in_any_key_field_or_input_is_a_miss() {
        type Change = fn(&mut HostKey, &mut String);
        let changes: [(&str, Change); 6] = [
            ("kernel", |h, _| h.kernel_release.push('+')),
            ("boot", |h, _| h.boot_id = "boot-b".into()),
            ("cpu", |h, _| h.cpu.push('1')),
            ("microcode", |h, _| h.microcode = "0xb101055".into()),
            ("hermit binary", |h, _| h.hermit_binary.push('1')),
            ("inputs", |_, i| i.push('+')),
        ];
        for (field, change) in changes {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("v.json");
            store(&path, &host(), "in", &7u32);
            let (mut changed, mut inputs) = (host(), "in".to_owned());
            change(&mut changed, &mut inputs);
            assert_eq!(load::<u32>(&path, &changed, &inputs), None, "{field}");
        }
    }

    #[test]
    fn an_older_schema_or_corrupt_file_is_a_miss() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v.json");
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load::<u32>(&path, &host(), "in"), None);
        store(&path, &host(), "in", &7u32);
        let mut older: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        older["schema"] = serde_json::json!(SCHEMA - 1);
        fs::write(&path, serde_json::to_vec(&older).unwrap()).unwrap();
        assert_eq!(load::<u32>(&path, &host(), "in"), None);
    }

    /// A FIFO at the cache path neither blocks the open nor counts as a
    /// verdict; an oversized file and a symbolic link are misses too.
    #[test]
    fn a_fifo_an_oversized_file_or_a_link_is_a_miss() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo.json");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        assert_eq!(load::<u32>(&fifo, &host(), "in"), None);
        // A valid entry padded past the cap with whitespace, which JSON
        // allows: only the size check, not the parse, refuses it.
        let big = directory.path().join("big.json");
        store(&big, &host(), "in", &7u32);
        let mut padded = fs::read(&big).unwrap();
        padded.resize(MAX_FILE_BYTES as usize + 1, b' ');
        fs::write(&big, &padded).unwrap();
        assert_eq!(load::<u32>(&big, &host(), "in"), None);
        let real = directory.path().join("real.json");
        store(&real, &host(), "in", &7u32);
        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(load::<u32>(&link, &host(), "in"), None);
    }

    #[test]
    fn an_unwritable_cache_stores_nothing_and_fails_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("hermit");
        fs::write(&blocker, b"a file where the directory would go").unwrap();
        let path = blocker.join("v.json");
        assert!(!store(&path, &host(), "in", &7u32));
        assert_eq!(load::<u32>(&path, &host(), "in"), None);
    }

    /// Each key field reads what it names, compared with an independent read
    /// of the same source: `/proc/sys/kernel/osrelease`, the boot id file,
    /// `/proc/cpuinfo`'s vendor, family, model, stepping and microcode, and
    /// this test binary's own metadata.
    #[test]
    fn the_current_key_reads_this_host() {
        use std::os::unix::fs::MetadataExt;
        let host = HostKey::current();
        assert_eq!(
            host.kernel_release,
            read_trimmed("/proc/sys/kernel/osrelease")
        );
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        assert_eq!(host.boot_id, boot.trim());
        assert!(!host.boot_id.is_empty());
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap();
        let field = |name: &str| {
            cpuinfo
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == name).then(|| value.trim().to_owned())
                })
                .unwrap_or_default()
        };
        #[cfg(target_arch = "x86_64")]
        {
            let number = |name: &str| field(name).parse::<u32>().unwrap();
            assert_eq!(
                host.cpu,
                format!(
                    "{} family {:#x} model {:#x} stepping {:#x}",
                    field("vendor_id"),
                    number("cpu family"),
                    number("model"),
                    number("stepping")
                )
            );
        }
        let sysfs = read_trimmed("/sys/devices/system/cpu/cpu0/microcode/version");
        assert_eq!(host.microcode, sysfs);
        if !field("microcode").is_empty() {
            assert_eq!(
                u64::from_str_radix(host.microcode.trim_start_matches("0x"), 16).unwrap(),
                u64::from_str_radix(field("microcode").trim_start_matches("0x"), 16).unwrap()
            );
        }
        let exe = fs::metadata(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(
            host.hermit_binary,
            format!(
                "{}:{}:{}:{}.{}",
                exe.dev(),
                exe.ino(),
                exe.size(),
                exe.mtime(),
                exe.mtime_nsec()
            )
        );
    }

    /// The signature decodes family, model and stepping as `/proc/cpuinfo`
    /// does, extended fields included.
    #[test]
    fn the_cpu_signature_decodes_cpuid_leaf_1() {
        let amd = [0x6874_7541, 0x6974_6e65, 0x444d_4163];
        assert_eq!(
            signature_of(amd, 0x00b4_0f15),
            "AuthenticAMD family 0x1a model 0x41 stepping 0x5"
        );
        let intel = [0x756e_6547, 0x4965_6e69, 0x6c65_746e];
        assert_eq!(
            signature_of(intel, 0x0008_06c1),
            "GenuineIntel family 0x6 model 0x8c stepping 0x1"
        );
    }

    /// A file another user owns is a miss.
    #[test]
    fn a_file_another_user_owns_is_a_miss() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v.json");
        store(&path, &host(), "in", &7u32);
        let me = nix::unistd::geteuid().as_raw();
        assert!(read_bounded_as(&path, me).is_some());
        assert!(read_bounded_as(&path, me.wrapping_add(1)).is_none());
    }

    #[test]
    fn writable_and_locked_report_and_serialize_the_cache_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hermit").join("v.json");
        assert!(writable(&path));
        assert_eq!(locked(&path, || 5), 5);
        assert!(path.with_extension("lock").is_file());
        let blocker = directory.path().join("blocked");
        fs::write(&blocker, b"a file where the directory would go").unwrap();
        assert!(!writable(&blocker.join("v.json")));
        assert_eq!(locked(&blocker.join("v.json"), || 6), 6);
    }
}
