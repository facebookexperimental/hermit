/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `--max-memory`: the most memory `hermit analyze` or `hermit bisect` holds
//! before it stops.

use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::app::container::PolicyRefusal;

/// How often the budget's watchdog reads the process's memory.
const WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// How much memory the process of `hermit analyze` or `hermit bisect` may
/// hold, resident or swapped out (`RssAnon` plus `VmSwap`): the schedules and
/// the search's working set. Each trial runs in a child process, which this
/// does not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBudget {
    /// A quarter of the memory the process may use: the host's, or the
    /// lowest limit of the cgroups it runs in.
    Auto,
    /// No budget.
    Off,
    /// This many bytes.
    Bytes(u64),
}

impl FromStr for MemoryBudget {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "auto" => return Ok(MemoryBudget::Auto),
            "off" => return Ok(MemoryBudget::Off),
            _ => {}
        }
        let (number, shift) = match text.as_bytes().last() {
            Some(b'K' | b'k') => (&text[..text.len() - 1], 10),
            Some(b'M' | b'm') => (&text[..text.len() - 1], 20),
            Some(b'G' | b'g') => (&text[..text.len() - 1], 30),
            Some(b'T' | b't') => (&text[..text.len() - 1], 40),
            _ => (text, 0),
        };
        number
            .parse::<u64>()
            .ok()
            .and_then(|count| count.checked_mul(1 << shift))
            .filter(|&bytes| bytes > 0)
            .map(MemoryBudget::Bytes)
            .ok_or_else(|| {
                format!(
                    "expected a nonzero size such as 16G or 512M (K, M, G and T are powers of \
                     1024) or a byte count, `auto`, or `off`; got {text:?}"
                )
            })
    }
}

impl MemoryBudget {
    /// The budget in bytes: `None` for `off`, and for `auto` where the memory
    /// the process may use cannot be read.
    pub fn bytes(self) -> Option<u64> {
        match self {
            MemoryBudget::Auto => usable_memory().map(|bytes| bytes / 4),
            MemoryBudget::Off => None,
            MemoryBudget::Bytes(bytes) => Some(bytes),
        }
    }

    /// Watch this process's memory from a thread, until the returned
    /// [`MemoryWatchdog`] is dropped, and end the process with the budget's
    /// refusal (exit 122) once it holds more. `command` (`hermit analyze`,
    /// `hermit bisect`) names it in the refusal. A watchdog, rather than a
    /// check between trials, also stops a computation that grows without
    /// returning. A running trial's container dies with the process: it is
    /// started with `PR_SET_PDEATHSIG`.
    ///
    /// Where `/proc/self/status` does not say how much the process holds,
    /// `auto` runs without a budget, with a warning, and an explicit size is
    /// refused.
    pub fn enforce(self, command: &'static str) -> anyhow::Result<MemoryWatchdog> {
        let Some(budget) = self.bytes() else {
            return Ok(MemoryWatchdog(None));
        };
        if held_memory().is_none() {
            let unreadable = "/proc/self/status does not say how much memory it holds";
            if self == MemoryBudget::Auto {
                let _ = writeln!(
                    std::io::stderr(),
                    ":: WARNING: {command} runs without a memory budget: {unreadable}"
                );
                return Ok(MemoryWatchdog(None));
            }
            return Err(anyhow::Error::new(PolicyRefusal).context(format!(
                "{command} cannot keep to --max-memory: {unreadable}"
            )));
        }
        let _ = writeln!(
            std::io::stderr(),
            ":: {command} stops if its own memory passes {} (--max-memory)",
            readable(budget)
        );
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        // Trials clone their containers from this process while the thread
        // runs, and only the cloning thread lives on in a container. So the
        // loop takes no lock, allocates nothing and leaves signal
        // dispositions alone (the rule `ContainerInitStopSignalBridge::install`
        // states); only a refusal allocates, as the process exits.
        std::thread::Builder::new()
            .name("memory-budget".to_owned())
            .spawn(move || {
                while !stopped.load(Ordering::Relaxed) {
                    if let Some(held) = held_memory()
                        && held > budget
                        && !stopped.load(Ordering::Relaxed)
                    {
                        crate::app::exit_with_error(refusal(command, held, budget));
                    }
                    std::thread::sleep(WATCH_INTERVAL);
                }
            })?;
        Ok(MemoryWatchdog(Some(stop)))
    }
}

/// Keeps a budget's watchdog running; dropping it stops the watchdog.
#[must_use = "dropping the watchdog stops it"]
pub struct MemoryWatchdog(Option<Arc<AtomicBool>>);

impl Drop for MemoryWatchdog {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

/// The refusal for `command` holding `held` bytes over its `budget`.
fn refusal(command: &str, held: u64, budget: u64) -> anyhow::Error {
    anyhow::Error::new(PolicyRefusal).context(format!(
        "{command} held {} of memory, more than its --max-memory budget of {}, so it stopped \
         instead of growing until the kernel kills it. The budget counts its own memory, \
         resident or swapped out (the schedules and the search), not the trials, which run in \
         child processes. Raise \
         --max-memory (or set it to `off`) on a host with memory to spare, or reproduce the \
         failure in a shorter run.",
        readable(held),
        readable(budget),
    ))
}

/// `bytes` in GiB to one decimal, or in whole MiB below 1 GiB.
fn readable(bytes: u64) -> String {
    if bytes < 1 << 30 {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
    }
}

/// The memory this process holds of its own (what its heap holds, without
/// the pages it maps from files): resident (`RssAnon`), plus what the kernel
/// swapped out (`VmSwap`, absent without swap accounting). Read into a buffer
/// on the stack (`File::open` takes a short path on the stack too), so the
/// watchdog allocates nothing.
fn held_memory() -> Option<u64> {
    let mut status = [0u8; 8192];
    let mut file = std::fs::File::open("/proc/self/status").ok()?;
    let mut filled = 0;
    while filled < status.len() {
        match file.read(&mut status[filled..]).ok()? {
            0 => break,
            read => filled += read,
        }
    }
    let status = &status[..filled];
    let resident = kibibyte_field(status, b"RssAnon:")?;
    Some(resident.saturating_add(kibibyte_field(status, b"VmSwap:").unwrap_or(0)))
}

/// The memory this process may use: the host's (`MemTotal`), or the lowest
/// `memory.max` of the cgroups (v2) it runs in.
fn usable_memory() -> Option<u64> {
    let host = kibibyte_field(&std::fs::read("/proc/meminfo").ok()?, b"MemTotal:")?;
    let membership = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    Some(usable(
        host,
        lowest_memory_max(Path::new("/sys/fs/cgroup"), &membership),
    ))
}

/// Of `host` bytes, how many a process may use under a cgroup limit, if any.
fn usable(host: u64, cgroup_limit: Option<u64>) -> u64 {
    cgroup_limit.map_or(host, |limit| limit.min(host))
}

/// The lowest `memory.max` of the cgroup (v2) that `membership`, in the form
/// of `/proc/self/cgroup`, names and of its ancestors in `hierarchy`.
fn lowest_memory_max(hierarchy: &Path, membership: &str) -> Option<u64> {
    let cgroup = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?;
    Path::new(cgroup.trim_start_matches('/'))
        .ancestors()
        .filter_map(|ancestor| {
            let path = hierarchy.join(ancestor).join("memory.max");
            std::fs::read_to_string(path)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .min()
}

/// The value of a `/proc` field such as `RssAnon:   1234 kB`, in bytes. Only
/// the value is read as text: the kernel prints a process's name (`Name:`)
/// unescaped, and the bytes of a long one can end mid-character.
fn kibibyte_field(text: &[u8], field: &[u8]) -> Option<u64> {
    let value = text
        .split(|&byte| byte == b'\n')
        .find_map(|line| line.strip_prefix(field))?;
    let kibibytes = std::str::from_utf8(value)
        .ok()?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse::<u64>()
        .ok()?;
    kibibytes.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_is_a_size_auto_or_off() {
        assert_eq!("auto".parse(), Ok(MemoryBudget::Auto));
        assert_eq!("off".parse(), Ok(MemoryBudget::Off));
        assert_eq!("4096".parse(), Ok(MemoryBudget::Bytes(4096)));
        assert_eq!("512M".parse(), Ok(MemoryBudget::Bytes(512 << 20)));
        assert_eq!("16g".parse(), Ok(MemoryBudget::Bytes(16 << 30)));
        assert_eq!("2T".parse(), Ok(MemoryBudget::Bytes(2 << 40)));
        for refused in ["0", "0G", "1.5G", "16GB", "-1", "", "G", "99999999999T"] {
            assert!(
                refused.parse::<MemoryBudget>().is_err(),
                "{refused:?} was accepted"
            );
        }
        assert_eq!(MemoryBudget::Off.bytes(), None);
        assert_eq!(MemoryBudget::Bytes(7).bytes(), Some(7));
    }

    /// A process may use the host's memory, or less when a cgroup limits it;
    /// `auto` is a quarter of that.
    #[test]
    fn a_cgroup_limit_below_the_host_memory_applies() {
        assert_eq!(usable(64 << 30, None), 64 << 30);
        assert_eq!(usable(64 << 30, Some(32 << 30)), 32 << 30);
        assert_eq!(usable(64 << 30, Some(128 << 30)), 64 << 30);
        assert!(MemoryBudget::Auto.bytes().is_some_and(|bytes| bytes > 0));
    }

    /// The limit is the lowest `memory.max` on the cgroup's path to the root
    /// of the hierarchy, which in a cgroup namespace has one too; `max` sets
    /// none, and without a cgroup v2 entry there is none.
    #[test]
    fn the_lowest_cgroup_limit_on_the_path_applies() {
        let hierarchy = tempfile::tempdir().unwrap();
        for (cgroup, max) in [
            ("user.slice", "68719476736"),
            ("user.slice/session.scope", "max"),
            ("user.slice/session.scope/run.scope", "34359738368"),
        ] {
            let dir = hierarchy.path().join(cgroup);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("memory.max"), format!("{max}\n")).unwrap();
        }
        let limit = |membership| lowest_memory_max(hierarchy.path(), membership);
        assert_eq!(
            limit("0::/user.slice/session.scope/run.scope\n"),
            Some(32 << 30)
        );
        assert_eq!(limit("0::/user.slice/session.scope\n"), Some(64 << 30));
        assert_eq!(limit("0::/\n"), None);
        assert_eq!(limit("4:memory:/user.slice\n"), None);
        std::fs::write(hierarchy.path().join("memory.max"), "8589934592\n").unwrap();
        assert_eq!(limit("0::/\n"), Some(8 << 30));
        assert_eq!(limit("0::/user.slice/session.scope\n"), Some(8 << 30));
    }

    /// The refusal is a policy refusal (exit 122) naming the command, what
    /// it held, and the budget.
    #[test]
    fn the_refusal_names_the_command_its_memory_and_the_budget() {
        let error = refusal("hermit bisect", 9 << 30, 8 << 30);
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert_eq!(crate::app::failure_exit_code(&error), 122);
        let message = format!("{error:#}");
        assert!(
            message.starts_with(
                "hermit bisect held 9.0 GiB of memory, more than its --max-memory budget of 8.0 GiB"
            ),
            "{message}"
        );
    }

    /// A field reads in bytes, beside a name that is not valid UTF-8 (the
    /// kernel cut a long one mid-character).
    #[test]
    fn a_proc_field_reads_in_bytes() {
        let status = b"Name:\thermit-\xc3\nRssAnon:\t  1024 kB\nRssFile:\t  2048 kB\n";
        assert_eq!(kibibyte_field(status, b"RssAnon:"), Some(1 << 20));
        assert_eq!(kibibyte_field(status, b"VmSwap:"), None);
        assert!(held_memory().is_some_and(|bytes| bytes > 0));
    }

    /// The marker under which [`growth_past_the_budget_ends_the_process`]
    /// starts this test binary again to run its body alone.
    const WATCHDOG_CHILD_ENV: &str = "HERMIT_MEMORY_BUDGET_WATCHDOG_CHILD";

    /// A process whose memory grows past its budget in one computation,
    /// without returning to any caller, is ended by the watchdog with the
    /// refusal and exit 122.
    #[test]
    fn growth_past_the_budget_ends_the_process() {
        // The harness names a test by its path below the crate root.
        let (_, module) = module_path!().split_once("::").unwrap();
        let name = format!("{module}::growth_past_the_budget_ends_the_process");
        if std::env::var(WATCHDOG_CHILD_ENV).as_deref() == Ok(name.as_str()) {
            let budget = 64 << 20;
            let _watchdog = MemoryBudget::Bytes(budget)
                .enforce("hermit analyze")
                .unwrap();
            let before = held_memory().unwrap();
            assert!(before < budget, "held {before} bytes before growing");
            println!("growing from {} MiB", before >> 20);
            // 16 MiB every 20 ms, up to four times the budget, then up to
            // 30 s in all for a watchdog starved of CPU on a loaded host.
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..1500 {
                if held.len() < 16 {
                    held.push(std::hint::black_box(vec![1; 16 << 20]));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("the watchdog let {} MiB through", held.len() * 16);
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name.as_str(), "--exact", "--nocapture", "--test-threads=1"])
            .env(WATCHDOG_CHILD_ENV, &name)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        let rendered = format!(
            "status {}\nstdout:\n{}\nstderr:\n{stderr}",
            output.status,
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(output.status.code(), Some(122), "{rendered}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("growing from"),
            "the refusal came before the growth:\n{rendered}"
        );
        assert!(
            stderr.contains("hermit analyze held")
                && stderr.contains("more than its --max-memory budget of 64 MiB")
                && stderr.contains("HERMIT_POLICY_REFUSAL"),
            "{rendered}"
        );
    }
}
