//! Launch observations for regular Nextest calibration. These observations do
//! not set limits or replace the prepared-artifact/source admission checks.
use std::collections::BTreeSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use crate::nextest_cpu::CpuQuota;
use crate::nextest_cpu::ExecutionCohort;
use crate::nextest_cpu::ExecutionDomain;
use crate::nextest_cpu::LaunchProofRef;
use crate::nextest_cpu::ResourceEnvelope;
use crate::nextest_cpu::ResourceLimit;

pub const PINNED_PROOF_PATH: &str = "/run/hermit-nextest-launch.json";
const MAX_PROOF: u64 = 262_144;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Anchor {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
enum CpusetInterface {
    Observed { cpus: Vec<u32> },
    Absent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Controls {
    #[serde(deserialize_with = "required_quota")]
    cpu: Option<CpuQuota>,
    cpuset: CpusetInterface,
    cpuset_enabled_for_children: bool,
    memory: ResourceLimit,
    swap: ResourceLimit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    anchor: Anchor,
    controls: Controls,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HostObservation {
    membership: String,
    namespace: String,
    affinity: Vec<u32>,
    /// Leaf first. The final entry is the host hierarchy root.
    ancestors: Vec<Layer>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
enum Observation {
    Unavailable {
        reason: String,
    },
    Complete {
        domain: ExecutionDomain,
        host: HostObservation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LaunchObservation {
    schema: u64,
    invocation: String,
    observation: Observation,
}

fn required_quota<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<CpuQuota>, D::Error> {
    Option::deserialize(deserializer)
}

fn anchor(file: &File) -> Result<Anchor, String> {
    let m = file.metadata().map_err(|e| e.to_string())?;
    Ok(Anchor {
        device: m.dev(),
        inode: m.ino(),
    })
}

fn directory(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot open cgroup directory {}: {e}", path.display()))?;
    let mut stat = unsafe { std::mem::zeroed::<libc::statfs>() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 || stat.f_type != 0x6367_7270 {
        return Err("launch resource observation requires a cgroup-v2 filesystem".into());
    }
    Ok(file)
}

fn child(directory: &File, name: &str, flags: i32) -> Result<File, std::io::Error> {
    let name = std::ffi::CString::new(name).expect("internal cgroup component contains no NUL");
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn bounded(file: File, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err("launch observation exceeds its bounded input size".into());
    }
    Ok(bytes)
}

fn control(directory: &File, name: &str, global_root: bool) -> Result<Option<String>, String> {
    let file = match child(directory, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if global_root && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(format!("cannot observe cgroup {name}: {error}")),
    };
    String::from_utf8(bounded(file, 16_384)?)
        .map(|v| Some(v.trim().into()))
        .map_err(|e| e.to_string())
}

fn limit(value: Option<String>) -> Result<ResourceLimit, String> {
    match value.as_deref() {
        None | Some("max") => Ok(ResourceLimit::Unlimited),
        Some(value) => value
            .parse::<u64>()
            .map(ResourceLimit::Max)
            .map_err(|e| format!("invalid cgroup resource limit: {e}")),
    }
}

pub fn cpu_set(value: &str) -> Result<Vec<u32>, String> {
    let mut result = BTreeSet::new();
    for part in value.split(',') {
        let (a, b) = part.split_once('-').unwrap_or((part, part));
        let a = a.parse::<u32>().map_err(|_| "invalid cgroup CPU set")?;
        let b = b.parse::<u32>().map_err(|_| "invalid cgroup CPU set")?;
        if b < a || b - a > 65_536 || b > 1_048_576 {
            return Err("invalid cgroup CPU range".into());
        }
        for cpu in a..=b {
            if !result.insert(cpu) {
                return Err("overlapping cgroup CPU ranges".into());
            }
        }
        if result.len() > 65_536 {
            return Err("cgroup CPU set exceeds supported observation size".into());
        }
    }
    if result.is_empty() {
        return Err("empty effective cgroup CPU set".into());
    }
    Ok(result.into_iter().collect())
}

fn controls(directory: &File, global_root: bool) -> Result<Controls, String> {
    let cpu = match control(directory, "cpu.max", global_root)? {
        None => None,
        Some(raw) => {
            let words = raw.split_whitespace().collect::<Vec<_>>();
            if words.len() != 2 {
                return Err("invalid cgroup cpu.max".into());
            }
            let period = words[1]
                .parse::<u64>()
                .map_err(|_| "invalid CPU quota period")?;
            if period == 0 {
                return Err("zero CPU quota period".into());
            }
            if words[0] == "max" {
                None
            } else {
                let quota = words[0].parse::<u64>().map_err(|_| "invalid CPU quota")?;
                if quota == 0 {
                    return Err("zero CPU quota".into());
                }
                Some(CpuQuota {
                    quota_usec: quota,
                    period_usec: period,
                })
            }
        }
    };
    Ok(Controls {
        cpu,
        cpuset: match child(directory, "cpuset.cpus.effective", libc::O_RDONLY) {
            Ok(file) => CpusetInterface::Observed {
                cpus: cpu_set(
                    String::from_utf8(bounded(file, 16_384)?)
                        .map_err(|e| e.to_string())?
                        .trim(),
                )?,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => CpusetInterface::Absent,
            Err(error) => return Err(format!("cannot observe cgroup cpuset: {error}")),
        },
        cpuset_enabled_for_children: cpuset_enabled(
            &control(directory, "cgroup.subtree_control", false)?
                .ok_or("missing cgroup controller-enable state")?,
        )?,
        memory: limit(control(directory, "memory.max", global_root)?)?,
        swap: limit(control(directory, "memory.swap.max", global_root)?)?,
    })
}

fn cpuset_enabled(raw: &str) -> Result<bool, String> {
    let mut controllers = BTreeSet::new();
    for name in raw.split_whitespace() {
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || !controllers.insert(name)
        {
            return Err("malformed cgroup controller-enable state".into());
        }
    }
    Ok(controllers.contains("cpuset"))
}

fn bounded_text(path: &str, limit: u64) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("cannot observe {path}: {e}"))?;
    String::from_utf8(bounded(file, limit)?).map_err(|e| e.to_string())
}

fn membership() -> Result<String, String> {
    let raw = bounded_text("/proc/self/cgroup", 16_384)?;
    let paths = raw
        .lines()
        .filter_map(|line| line.strip_prefix("0::"))
        .collect::<Vec<_>>();
    if paths.len() != 1
        || !paths[0].starts_with('/')
        || Path::new(paths[0])
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err("cannot establish a unified cgroup-v2 membership".into());
    }
    Ok(paths[0].into())
}

fn affinity() -> Result<Vec<u32>, String> {
    let mut set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&set), &mut set) } != 0 {
        return Err(format!(
            "cannot observe CPU affinity: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok((0..libc::CPU_SETSIZE)
        .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu as usize, &set) })
        .map(|cpu| cpu as u32)
        .collect())
}

fn is_container() -> bool {
    Path::new("/run/.containerenv").exists() || Path::new("/.dockerenv").exists()
}

fn observe_host() -> Result<HostObservation, String> {
    if is_container() {
        return Err("native launch observation is unavailable inside a container".into());
    }
    let membership = membership()?;
    let root = directory(Path::new("/sys/fs/cgroup"))?;
    let root_id = anchor(&root)?;
    // Linux cgroup-v2's global kernfs root is inode 1. A private cgroup
    // namespace exposes its leaf with another inode even when mountinfo says
    // root "/". Never interpret missing controls as unlimited on that leaf.
    if root_id.inode != 1 {
        return Err("global host cgroup root identity is unavailable".into());
    }
    // The maintained native launch boundary supplies host authority. Require
    // its global cgroup mount, not a namespaced leaf presented at the same path.
    let mounts = bounded_text("/proc/self/mountinfo", 1_048_576)?;
    if !mounts.lines().any(|line| {
        let words = line.split_whitespace().collect::<Vec<_>>();
        words.get(3) == Some(&"/")
            && words.get(4) == Some(&"/sys/fs/cgroup")
            && line.contains(" - cgroup2 ")
    }) {
        return Err("complete host cgroup hierarchy is unavailable".into());
    }
    let mut current = directory(Path::new("/sys/fs/cgroup"))?;
    for part in Path::new(&membership).components() {
        if let Component::Normal(name) = part {
            current = child(
                &current,
                name.to_str().ok_or("non-UTF-8 cgroup component")?,
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .map_err(|e| e.to_string())?;
        }
    }
    let mut ancestors = Vec::new();
    for _ in 0..64 {
        let id = anchor(&current)?;
        let global_root = id == root_id;
        ancestors.push(Layer {
            anchor: id,
            controls: controls(&current, global_root)?,
        });
        if global_root {
            return Ok(HostObservation {
                membership,
                namespace: fs::read_link("/proc/self/ns/cgroup")
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .into_owned(),
                affinity: affinity()?,
                ancestors,
            });
        }
        current =
            child(&current, "..", libc::O_RDONLY | libc::O_DIRECTORY).map_err(|e| e.to_string())?;
    }
    Err("host cgroup hierarchy exceeds the bounded observation depth".into())
}

fn minimum(a: ResourceLimit, b: &ResourceLimit) -> ResourceLimit {
    match (a, b) {
        (ResourceLimit::Max(a), ResourceLimit::Max(b)) => ResourceLimit::Max(a.min(*b)),
        (ResourceLimit::Unlimited, b) => b.clone(),
        (a, ResourceLimit::Unlimited) => a,
    }
}

fn cohort(domain: &ExecutionDomain, host: &HostObservation) -> Result<ExecutionCohort, String> {
    if host.ancestors.is_empty()
        || host.ancestors.len() > 64
        || host.namespace.is_empty()
        || !host.membership.starts_with('/')
        || host
            .ancestors
            .last()
            .is_none_or(|layer| layer.anchor.inode != 1)
        || host
            .ancestors
            .iter()
            .map(|layer| (layer.anchor.device, layer.anchor.inode))
            .collect::<BTreeSet<_>>()
            .len()
            != host.ancestors.len()
    {
        return Err("malformed complete launch observation".into());
    }
    let mut cpu = BTreeSet::new();
    let mut memory = ResourceLimit::Unlimited;
    let mut swap = ResourceLimit::Unlimited;
    let mut effective_cpuset = None;
    // Traverse the authenticated hierarchy from its global root to the leaf.
    // A child lacks cpuset interfaces when its parent has not enabled that
    // controller. Retain that absence in the proof; it is not an empty or
    // unrestricted effective set, and task affinity is a separate observation.
    for (index, layer) in host.ancestors.iter().enumerate().rev() {
        if layer.anchor.device == 0 || layer.anchor.inode == 0 {
            return Err("malformed ancestor in complete launch observation".into());
        }
        match &layer.controls.cpuset {
            CpusetInterface::Observed { cpus } => {
                if host
                    .ancestors
                    .get(index + 1)
                    .is_some_and(|parent| !parent.controls.cpuset_enabled_for_children)
                {
                    return Err("cpuset interface exists beneath a disabled controller".into());
                }
                if cpus.is_empty()
                    || cpus.len() > 65_536
                    || cpus.last().is_some_and(|cpu| *cpu > 1_048_576)
                    || cpus.windows(2).any(|pair| pair[0] >= pair[1])
                    || effective_cpuset.as_ref().is_some_and(|parent: &Vec<u32>| {
                        cpus.iter().any(|cpu| parent.binary_search(cpu).is_err())
                    })
                {
                    return Err("malformed ancestor effective cpuset".into());
                }
                effective_cpuset = Some(cpus.clone());
            }
            CpusetInterface::Absent => {
                if layer.controls.cpuset_enabled_for_children {
                    return Err(
                        "absent local cpuset cannot enable the controller for children".into(),
                    );
                }
                let parent = host
                    .ancestors
                    .get(index + 1)
                    .ok_or("absent cpuset has no authenticated ancestor authority")?;
                if parent.controls.cpuset_enabled_for_children {
                    return Err("missing cpuset interface beneath an enabled controller".into());
                }
                if effective_cpuset.is_none() {
                    return Err("absent cpuset has no observed ancestor effective set".into());
                }
            }
        }
        if let Some(limit) = &layer.controls.cpu {
            cpu.insert(limit.clone());
        }
        memory = minimum(memory, &layer.controls.memory);
        swap = minimum(swap, &layer.controls.swap);
    }
    let value = ExecutionCohort {
        schema: 1,
        domain: domain.clone(),
        resources: ResourceEnvelope {
            cpu_limits: cpu.into_iter().collect(),
            affinity: host.affinity.clone(),
            cpuset: effective_cpuset.ok_or("no observed effective cpuset")?,
            memory_bytes: memory,
            swap_bytes: swap,
        },
    };
    value.validate()?;
    if value
        .resources
        .affinity
        .iter()
        .any(|cpu| value.resources.cpuset.binary_search(cpu).is_err())
    {
        return Err("task affinity lies outside the observed effective cpuset".into());
    }
    Ok(value)
}

/// Called only at the explicit maintained native launch boundary. Never
/// executes a compiler, creates a cgroup or changes a resource control.
pub fn capture(domain: ExecutionDomain, output: &Path) -> Result<(), String> {
    domain.validate()?;
    if !output.is_absolute() {
        return Err("launch proof requires a fresh absolute output path".into());
    }
    let observation = capture_observation(domain, observe_host)?;
    let bytes = serde_json::to_vec(&LaunchObservation {
        schema: 2,
        invocation: output.to_string_lossy().into_owned(),
        observation,
    })
    .map_err(|e| e.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .custom_flags(libc::O_NOFOLLOW)
        .open(output)
        .map_err(|e| {
            format!(
                "cannot exclusively create launch proof {}: {e}; use a fresh per-invocation path",
                output.display()
            )
        })?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

fn capture_observation(
    domain: ExecutionDomain,
    mut observe: impl FnMut() -> Result<HostObservation, String>,
) -> Result<Observation, String> {
    Ok(match observe() {
        Ok(first) => match observe() {
            Ok(second) if first == second => {
                cohort(&domain, &first)?;
                Observation::Complete {
                    domain,
                    host: first,
                }
            }
            Ok(_) => Observation::Unavailable {
                reason: "cgroup membership or limits changed during launch capture".into(),
            },
            Err(reason) => Observation::Unavailable { reason },
        },
        Err(reason) => Observation::Unavailable { reason },
    })
}

fn container_image() -> Result<(String, String), String> {
    let raw = bounded_text("/run/.containerenv", 16_384)?;
    let field = |key: &str| -> Result<String, String> {
        let prefix = format!("{key}=\"");
        let values = raw
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix).and_then(|v| v.strip_suffix('"')))
            .collect::<Vec<_>>();
        if values.len() != 1 {
            return Err(format!("Podman runtime evidence has no unique {key}"));
        }
        Ok(values[0].into())
    };
    Ok((field("image")?, field("imageid")?))
}

pub fn verify(path: &Path) -> Result<(Option<ExecutionCohort>, Option<LaunchProofRef>), String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((None, None)),
        Err(e) => return Err(format!("cannot read launch proof {}: {e}", path.display())),
    };
    let before = file.metadata().map_err(|e| e.to_string())?;
    if !before.is_file() || before.len() == 0 || before.len() > MAX_PROOF {
        return Err("launch proof must be a bounded nonempty regular file".into());
    }
    let bytes = bounded(file.try_clone().map_err(|e| e.to_string())?, MAX_PROOF)?;
    let after = file.metadata().map_err(|e| e.to_string())?;
    if (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) != (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    ) || bytes.len() as u64 != before.len()
    {
        return Err("launch proof changed while read".into());
    }
    let proof: LaunchObservation =
        serde_json::from_slice(&bytes).map_err(|e| format!("malformed launch proof: {e}"))?;
    if !matches!(proof.schema, 1 | 2) || !Path::new(&proof.invocation).is_absolute() {
        return Err("unsupported or malformed launch proof identity".into());
    }
    let value = match proof.observation {
        Observation::Unavailable { reason } => {
            if reason.trim().is_empty() {
                return Err("unavailable launch observation requires a reason".into());
            }
            None
        }
        Observation::Complete { domain, host } => {
            if proof.schema != 2 {
                return Err(
                    "complete launch proof requires current controller-enable evidence".into(),
                );
            }
            let value = cohort(&domain, &host)?;
            match &domain {
                ExecutionDomain::Host => {
                    if observe_host()? != host {
                        return Err("native launch membership or resource limits changed; capture a fresh launch proof".into());
                    }
                }
                ExecutionDomain::PinnedRoot { image, image_id } => {
                    // Only the wrapper's dedicated read-only mount conveys
                    // hidden host ancestors. A retained copy is evidence, not
                    // authority, even if its visible leaf/image happen to match.
                    if path != Path::new(PINNED_PROOF_PATH) {
                        return Err("pinned launch proof must come from the maintained wrapper's fixed read-only mount".into());
                    }
                    let mounts = bounded_text("/proc/self/mountinfo", 1_048_576)?;
                    let entries = mounts
                        .lines()
                        .filter_map(|line| {
                            let words = line.split_whitespace().collect::<Vec<_>>();
                            (words.get(4) == Some(&PINNED_PROOF_PATH)).then_some(words)
                        })
                        .collect::<Vec<_>>();
                    if entries.len() != 1
                        || !entries[0]
                            .get(5)
                            .is_some_and(|flags| flags.split(',').any(|flag| flag == "ro"))
                    {
                        return Err("pinned launch proof has no unique dedicated read-only mount; use the maintained wrapper".into());
                    }
                    if container_image()? != (image.clone(), image_id.clone()) {
                        return Err(
                            "actual Podman image differs from the launch observation".into()
                        );
                    }
                    let root = directory(Path::new("/sys/fs/cgroup"))?;
                    if membership()? != "/"
                        || anchor(&root)? != host.ancestors[0].anchor
                        || controls(&root, false)? != host.ancestors[0].controls
                        || affinity()? != host.affinity
                    {
                        return Err("pinned launch cgroup anchor or visible limits differ from the host observation".into());
                    }
                    // Outer ancestors were observed by the host launcher. They
                    // are hidden here; do not claim an inner live recheck.
                }
            }
            Some(value)
        }
    };
    Ok((
        value,
        Some(LaunchProofRef {
            path: path.to_string_lossy().into_owned(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inherited_host() -> HostObservation {
        let layer = |inode, cpuset, enabled| Layer {
            anchor: Anchor { device: 27, inode },
            controls: Controls {
                cpu: None,
                cpuset,
                cpuset_enabled_for_children: enabled,
                memory: ResourceLimit::Unlimited,
                swap: ResourceLimit::Unlimited,
            },
        };
        HostObservation {
            membership: "/slice/leaf".into(),
            namespace: "cgroup:[123]".into(),
            affinity: vec![1, 2],
            ancestors: vec![
                layer(30, CpusetInterface::Absent, false),
                layer(
                    20,
                    CpusetInterface::Observed {
                        cpus: vec![0, 1, 2],
                    },
                    false,
                ),
                layer(
                    1,
                    CpusetInterface::Observed {
                        cpus: vec![0, 1, 2, 3],
                    },
                    true,
                ),
            ],
        }
    }

    #[test]
    fn disabled_cpuset_inherits_the_nearest_authenticated_effective_set() {
        let mut host = inherited_host();
        let inherited = cohort(&ExecutionDomain::Host, &host).unwrap();
        assert_eq!(inherited.resources.cpuset, vec![0, 1, 2]);
        assert_eq!(inherited.resources.affinity, vec![1, 2]);
        assert_eq!(host.ancestors[0].controls.cpuset, CpusetInterface::Absent);
        host.ancestors[0].controls.cpuset = CpusetInterface::Observed {
            cpus: vec![0, 1, 2],
        };
        host.ancestors[1].controls.cpuset_enabled_for_children = true;
        assert_eq!(cohort(&ExecutionDomain::Host, &host).unwrap(), inherited);

        host.ancestors[0].controls.cpuset = CpusetInterface::Absent;
        host.ancestors[1].controls.cpuset = CpusetInterface::Absent;
        host.ancestors[1].controls.cpuset_enabled_for_children = false;
        host.ancestors[2].controls.cpuset_enabled_for_children = false;
        assert_eq!(
            cohort(&ExecutionDomain::Host, &host)
                .unwrap()
                .resources
                .cpuset,
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn inherited_cpuset_refuses_missing_or_contradictory_authority() {
        let mut host = inherited_host();
        host.ancestors[0].controls.cpuset = CpusetInterface::Observed {
            cpus: vec![0, 1, 2],
        };
        assert_eq!(
            cohort(&ExecutionDomain::Host, &host).unwrap_err(),
            "cpuset interface exists beneath a disabled controller"
        );
        host = inherited_host();
        host.ancestors[1].controls.cpuset_enabled_for_children = true;
        assert_eq!(
            cohort(&ExecutionDomain::Host, &host).unwrap_err(),
            "missing cpuset interface beneath an enabled controller"
        );
        host = inherited_host();
        host.ancestors[0].controls.cpuset_enabled_for_children = true;
        assert_eq!(
            cohort(&ExecutionDomain::Host, &host).unwrap_err(),
            "absent local cpuset cannot enable the controller for children"
        );
        host = inherited_host();
        host.ancestors[2].controls.cpuset = CpusetInterface::Absent;
        host.ancestors[2].controls.cpuset_enabled_for_children = false;
        assert_eq!(
            cohort(&ExecutionDomain::Host, &host).unwrap_err(),
            "absent cpuset has no authenticated ancestor authority"
        );
        for cpus in [vec![], vec![1, 1], vec![2, 1], vec![0, 4], vec![1_048_577]] {
            host = inherited_host();
            host.ancestors[1].controls.cpuset = CpusetInterface::Observed { cpus };
            assert_eq!(
                cohort(&ExecutionDomain::Host, &host).unwrap_err(),
                "malformed ancestor effective cpuset"
            );
        }
        host = inherited_host();
        host.affinity.push(3);
        assert_eq!(
            cohort(&ExecutionDomain::Host, &host).unwrap_err(),
            "task affinity lies outside the observed effective cpuset"
        );
        let mut raw = serde_json::to_value(inherited_host()).unwrap();
        raw["ancestors"][1]["controls"]
            .as_object_mut()
            .unwrap()
            .remove("cpuset_enabled_for_children");
        assert!(serde_json::from_value::<HostObservation>(raw).is_err());
    }

    #[test]
    fn local_cpuset_observation_preserves_absence_and_refuses_read_errors() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!(
            "hermit-cpuset-controls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let temp = Scratch(path);
        let root = &temp.0;
        for (name, value) in [
            ("cpu.max", "max 100000"),
            ("memory.max", "max"),
            ("memory.swap.max", "0"),
            ("cgroup.subtree_control", "cpu memory pids"),
        ] {
            fs::write(root.join(name), value).unwrap();
        }
        let file = File::open(root).unwrap();
        let absent = controls(&file, false).unwrap();
        assert_eq!(absent.cpuset, CpusetInterface::Absent);
        assert!(!absent.cpuset_enabled_for_children);
        let path = root.join("cpuset.cpus.effective");
        fs::write(&path, "0-3\n").unwrap();
        let present = controls(&file, false).unwrap();
        assert_eq!(
            present.cpuset,
            CpusetInterface::Observed {
                cpus: vec![0, 1, 2, 3]
            }
        );
        assert_ne!(present, absent);
        for value in ["", "0,0", "3-1", "max"] {
            fs::write(&path, value).unwrap();
            assert!(controls(&file, false).is_err(), "accepted {value:?}");
        }
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("cpu.max", &path).unwrap();
        assert!(
            controls(&file, false)
                .unwrap_err()
                .contains("cannot observe cgroup cpuset")
        );
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(controls(&file, false).is_err());
        fs::remove_dir(&path).unwrap();
        fs::remove_file(root.join("cgroup.subtree_control")).unwrap();
        assert!(controls(&file, false).is_err());
        for raw in ["cpuset cpuset", "+cpuset", "cpuset/cpu"] {
            assert!(cpuset_enabled(raw).is_err());
        }
        assert!(cpuset_enabled("cpu cpuset memory pids\n").unwrap());
        assert!(!cpuset_enabled("").unwrap());
    }

    #[test]
    fn changed_cpuset_capture_never_becomes_complete() {
        let first = inherited_host();
        assert!(matches!(
            capture_observation(ExecutionDomain::Host, || Ok(first.clone())).unwrap(),
            Observation::Complete { .. }
        ));
        let mut changes = Vec::new();
        let mut second = first.clone();
        second.ancestors[0].controls.cpuset = CpusetInterface::Observed {
            cpus: vec![0, 1, 2],
        };
        changes.push(second);
        second = first.clone();
        second.ancestors[1].controls.cpuset_enabled_for_children = true;
        changes.push(second);
        second = first.clone();
        second.ancestors[1].controls.cpuset = CpusetInterface::Observed { cpus: vec![1, 2] };
        changes.push(second);
        for second in changes {
            let mut observations = [Ok(first.clone()), Ok(second)].into_iter();
            assert_eq!(
                capture_observation(ExecutionDomain::Host, || observations.next().unwrap())
                    .unwrap(),
                Observation::Unavailable {
                    reason: "cgroup membership or limits changed during launch capture".into()
                }
            );
        }
        let mut observations = [Ok(first), Err("unreadable ancestor".into())].into_iter();
        assert_eq!(
            capture_observation(ExecutionDomain::Host, || observations.next().unwrap()).unwrap(),
            Observation::Unavailable {
                reason: "unreadable ancestor".into()
            }
        );
    }
}
