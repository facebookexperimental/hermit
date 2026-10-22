//! Bounded guest logs and stall classification for long pressure runs.
//!
//! `pressure-test.rs run --log-tail-bytes BYTES` runs [`Warden`] beside the
//! scheduler. Every `--stall-sample-seconds` (default 30) it visits each cell
//! run directory `RESULTS/runs/<run id>/`, where the harness retains a cell's
//! verify logs and captures (`E2E_RESULT_ROOT`, `E2E_KEEP_VERIFY_LOGS=1`):
//!
//! - **Bounded tail.** A file larger than [`PUNCH_FACTOR`] x BYTES keeps only
//!   its last BYTES on disk: the head is punched out with
//!   `fallocate --punch-hole --keep-size`, so the file keeps its logical size
//!   (the running byte count), Hermit keeps appending at its offset, and the
//!   head reads as zeros. Disk use per file stays under `PUNCH_FACTOR` x BYTES
//!   whatever the run's length. A run whose logs were punched no longer has
//!   intact verify evidence; [`StallRecord::log_truncated`] says so and the
//!   summary refuses the row as evidence.
//! - **Samples.** The logical bytes of the run's files, the CPU time of every
//!   live process whose environment names `E2E_RUN_ID=<run id>` (utime, stime
//!   and reaped children's), and how many normalized lines of the newest log's
//!   last [`FINGERPRINT_BYTES`] were never seen before in this run.
//!
//! When the run's processes are gone (or the warden stops) it writes
//! `RESULTS/runs/<run id>/stall.json` ([`StallRecord`]). [`classify`] judges
//! the last [`WINDOW_SAMPLES`] samples, the same rules as the 2026-10-11
//! one-hour timeout study (195 timeout-red cells: 178 finished, 10 deadlocked,
//! 7 spun silently):
//!
//! | class | rule over the window |
//! |---|---|
//! | `deadlock` | log bytes flat and CPU below 1% of one core |
//! | `livelock-silent` | CPU at least 10% of one core, log bytes flat |
//! | `livelock` | CPU at least 10% of one core, log growing, no new distinct line |
//! | `still-progressing` | CPU and log growing with new distinct lines (slow, not stuck) |
//! | `unclear` | anything else, or fewer than 3 live samples |

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::SeekFrom;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

pub const STALL_SCHEMA: &str = "pressure-stall/v1";
pub const STALL_FILE: &str = "stall.json";
/// A file is punched once it exceeds this many times the tail budget, so a
/// punch frees at least three tails' worth and disk use stays bounded.
pub const PUNCH_FACTOR: u64 = 4;
/// Smallest accepted `--log-tail-bytes`: a tail must still hold the last
/// screens of a verify log to explain a stall.
pub const MIN_TAIL_BYTES: u64 = 1 << 20;
pub const DEFAULT_SAMPLE_SECONDS: u64 = 30;
/// 20 samples: 10 minutes at the default interval, the window the study used.
pub const WINDOW_SAMPLES: usize = 20;
const FINGERPRINT_BYTES: u64 = 64 << 10;
const PAGE: u64 = 4096;

#[derive(Clone, Debug)]
pub struct Options {
    pub tail_bytes: u64,
    pub interval: Duration,
}

/// Parse the value of `--log-tail-bytes`.
pub fn parse_tail_bytes(text: &str) -> Result<u64, String> {
    let bytes: u64 = text
        .parse()
        .map_err(|_| format!("--log-tail-bytes requires a byte count, got `{text}`"))?;
    if bytes < MIN_TAIL_BYTES {
        return Err(format!(
            "--log-tail-bytes {bytes} is below the {MIN_TAIL_BYTES}-byte minimum"
        ));
    }
    Ok(bytes)
}

/// Parse the value of `--stall-sample-seconds`.
pub fn parse_sample_seconds(text: &str) -> Result<u64, String> {
    match text.parse::<u64>() {
        Ok(seconds) if (1..=3600).contains(&seconds) => Ok(seconds),
        _ => Err(format!(
            "--stall-sample-seconds requires 1 to 3600 seconds, got `{text}`"
        )),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds since the warden started.
    pub t: u64,
    pub log_bytes: u64,
    pub cpu_usec: Option<u64>,
    pub alive: bool,
    pub new_distinct_lines: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StallRecord {
    pub schema: String,
    pub run_id: String,
    pub tail_bytes: u64,
    pub sample_seconds: u64,
    pub samples: usize,
    pub log_bytes: u64,
    /// Bytes punched out of the run's files; nonzero means its retained
    /// logs are not intact.
    pub punched_bytes: u64,
    pub log_truncated: bool,
    pub class: String,
    pub facts: BTreeMap<String, serde_json::Value>,
}

#[derive(Default)]
struct RunState {
    samples: Vec<Sample>,
    seen_lines: BTreeSet<u64>,
    /// Per file: how far its head has been punched.
    punched: BTreeMap<PathBuf, u64>,
    was_alive: bool,
    written: bool,
}

impl RunState {
    fn punched_bytes(&self) -> u64 {
        self.punched.values().sum()
    }
}

/// The background sampler. Dropping it without [`Warden::finish`] still stops it.
pub struct Warden {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Warden {
    pub fn start(results: &Path, options: Options) -> Warden {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let runs = results.join("runs");
        let handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut states = BTreeMap::<String, RunState>::new();
            loop {
                let stopping = flag.load(Ordering::SeqCst);
                tick(
                    &runs,
                    &options,
                    started.elapsed().as_secs(),
                    &mut states,
                    stopping,
                );
                if stopping {
                    break;
                }
                let deadline = Instant::now() + options.interval;
                while Instant::now() < deadline && !flag.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        });
        Warden {
            stop,
            handle: Some(handle),
        }
    }

    /// Take a last sample, write every run's record, and wait for the thread.
    pub fn finish(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Warden {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn tick(
    runs: &Path,
    options: &Options,
    t: u64,
    states: &mut BTreeMap<String, RunState>,
    stopping: bool,
) {
    let Ok(entries) = fs::read_dir(runs) else {
        return;
    };
    let cpu = run_cpu_usec();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let run_id = entry.file_name().to_string_lossy().into_owned();
        let state = states.entry(run_id.clone()).or_default();
        if state.written {
            continue;
        }
        let dir = entry.path();
        let (log_bytes, newest) = bound_files(&dir, options.tail_bytes, &mut state.punched);
        let new_distinct_lines = newest
            .map(|path| new_lines(&path, &mut state.seen_lines))
            .unwrap_or(0);
        let cpu_usec = cpu.get(&run_id).copied();
        let alive = cpu_usec.is_some();
        state.samples.push(Sample {
            t,
            log_bytes,
            cpu_usec,
            alive,
            new_distinct_lines,
        });
        state.was_alive |= alive;
        if (state.was_alive && !alive) || stopping {
            let record = record(&run_id, options, state);
            if let Ok(text) = serde_json::to_string_pretty(&record) {
                let _ = fs::write(dir.join(STALL_FILE), text + "\n");
            }
            state.written = true;
        }
    }
}

/// Bound every regular file under DIR; return their logical bytes and the newest file.
fn bound_files(
    dir: &Path,
    tail_bytes: u64,
    punched: &mut BTreeMap<PathBuf, u64>,
) -> (u64, Option<PathBuf>) {
    let mut total = 0;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() || path.file_name().is_some_and(|name| name == STALL_FILE) {
                continue;
            }
            total += metadata.len();
            if let Ok(modified) = metadata.modified() {
                if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
                    newest = Some((modified, path.clone()));
                }
            }
            if metadata.len() > PUNCH_FACTOR * tail_bytes {
                let target = (metadata.len() - tail_bytes) / PAGE * PAGE;
                let done = punched.get(&path).copied().unwrap_or(0);
                if target > done && punch_head(&path, target).is_ok() {
                    punched.insert(path, target);
                }
            }
        }
    }
    (total, newest.map(|(_, path)| path))
}

/// Deallocate the first LENGTH bytes of PATH without changing its size.
pub fn punch_head(path: &Path, length: u64) -> Result<(), String> {
    let status = Command::new("fallocate")
        .args(["--punch-hole", "--keep-size", "--offset", "0", "--length"])
        .arg(length.to_string())
        .arg(path)
        .status()
        .map_err(|error| format!("cannot run fallocate: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "fallocate --punch-hole failed on {}: {status}",
            path.display()
        ))
    }
}

/// Count the normalized lines of PATH's last bytes that SEEN does not hold, and add them.
fn new_lines(path: &Path, seen: &mut BTreeSet<u64>) -> u64 {
    let Ok(mut file) = fs::File::open(path) else {
        return 0;
    };
    let length = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let start = length.saturating_sub(FINGERPRINT_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return 0;
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return 0;
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // a partial line
    }
    let mut new = 0;
    for line in lines {
        let normalized = normalize(line);
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(fnv1a(normalized.as_bytes())) {
            new += 1;
        }
    }
    new
}

/// A log line with every run of digits (and 0x hex) replaced, so a line that
/// differs only by a counter, address or timestamp is not new.
pub fn normalize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '0'
            && chars
                .peek()
                .is_some_and(|next| *next == 'x' || *next == 'X')
        {
            chars.next();
            while chars.peek().is_some_and(char::is_ascii_hexdigit) {
                chars.next();
            }
            out.push('#');
        } else if ch.is_ascii_digit() {
            while chars.peek().is_some_and(char::is_ascii_digit) {
                chars.next();
            }
            out.push('#');
        } else if ch != '\0' {
            out.push(ch);
        }
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

/// CPU microseconds per E2E_RUN_ID over every live process that names one.
pub fn run_cpu_usec() -> BTreeMap<String, u64> {
    let ticks = clock_ticks();
    let mut totals = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return totals;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|text| text.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(environment) = fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        let Some(run_id) = environment
            .split(|byte| *byte == 0)
            .find_map(|item| item.strip_prefix(b"E2E_RUN_ID="))
            .map(|value| String::from_utf8_lossy(value).into_owned())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(rest) = stat.rfind(')').map(|index| &stat[index + 2..]) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // utime, stime, cutime, cstime are fields 14-17, here 11-14.
        let used: u64 = fields
            .get(11..15)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.parse::<u64>().ok())
                    .sum()
            })
            .unwrap_or(0);
        *totals.entry(run_id).or_insert(0) += used * 1_000_000 / ticks;
    }
    totals
}

fn clock_ticks() -> u64 {
    Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .filter(|ticks: &u64| *ticks > 0)
        .unwrap_or(100)
}

fn record(run_id: &str, options: &Options, state: &RunState) -> StallRecord {
    let (class, facts) = classify(&state.samples, options.interval.as_secs());
    let punched_bytes = state.punched_bytes();
    StallRecord {
        schema: STALL_SCHEMA.into(),
        run_id: run_id.into(),
        tail_bytes: options.tail_bytes,
        sample_seconds: options.interval.as_secs(),
        samples: state.samples.len(),
        log_bytes: state.samples.last().map_or(0, |sample| sample.log_bytes),
        punched_bytes,
        log_truncated: punched_bytes > 0,
        class: class.into(),
        facts,
    }
}

/// Judge the last [`WINDOW_SAMPLES`] live samples.
pub fn classify(
    samples: &[Sample],
    sample_seconds: u64,
) -> (&'static str, BTreeMap<String, serde_json::Value>) {
    let live: Vec<&Sample> = samples
        .iter()
        .filter(|sample| sample.alive && sample.cpu_usec.is_some())
        .collect();
    let mut facts = BTreeMap::new();
    if live.len() < 3 {
        facts.insert("reason".into(), "fewer than 3 live samples".into());
        return ("unclear", facts);
    }
    let window = &live[live.len().saturating_sub(WINDOW_SAMPLES)..];
    let (first, last) = (window[0], window[window.len() - 1]);
    let span = last.t.saturating_sub(first.t).max(sample_seconds.min(1));
    let cpu_seconds = last
        .cpu_usec
        .unwrap_or(0)
        .saturating_sub(first.cpu_usec.unwrap_or(0)) as f64
        / 1e6;
    let log_growth = last.log_bytes.saturating_sub(first.log_bytes);
    let new_lines: u64 = window[1..]
        .iter()
        .map(|sample| sample.new_distinct_lines)
        .sum();
    let cores = cpu_seconds / span as f64;
    facts.insert("window_seconds".into(), span.into());
    facts.insert(
        "cpu_seconds".into(),
        ((cpu_seconds * 10.0).round() / 10.0).into(),
    );
    facts.insert("cores".into(), ((cores * 1000.0).round() / 1000.0).into());
    facts.insert("log_bytes".into(), log_growth.into());
    facts.insert("new_distinct_lines".into(), new_lines.into());
    let class = if log_growth == 0 && cores < 0.01 {
        "deadlock"
    } else if cores >= 0.1 && log_growth == 0 {
        "livelock-silent"
    } else if cores >= 0.1 && log_growth > 0 && new_lines == 0 {
        "livelock"
    } else if cpu_seconds > 0.0 && log_growth > 0 && new_lines > 0 {
        "still-progressing"
    } else {
        "unclear"
    };
    (class, facts)
}

/// RESULTS/runs/RUN_ID/stall.json, when the warden wrote one.
pub fn read_record(results: &Path, run_id: &str) -> Result<Option<StallRecord>, String> {
    let path = results.join("runs").join(run_id).join(STALL_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let record: StallRecord = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    if record.schema != STALL_SCHEMA || record.run_id != run_id {
        return Err(format!(
            "{} is not the {STALL_SCHEMA} record of {run_id}",
            path.display()
        ));
    }
    Ok(Some(record))
}

/// The summary row's `stall` value: the class only for a timed-out row.
pub fn summary_value(record: &StallRecord, timed_out: bool) -> serde_json::Value {
    serde_json::json!({
        "class": if timed_out { record.class.as_str() } else { "finished" },
        "facts": if timed_out { serde_json::to_value(&record.facts).unwrap_or_default() } else { serde_json::json!({}) },
        "log_bytes": record.log_bytes,
        "punched_bytes": record.punched_bytes,
        "log_truncated": record.log_truncated,
        "tail_bytes": record.tail_bytes,
        "samples": record.samples,
    })
}

pub fn self_test(scratch: &Path) -> Result<(), String> {
    // Options.
    if parse_tail_bytes("1048575").is_ok() || parse_tail_bytes("x").is_ok() {
        return Err("--log-tail-bytes accepted a value below the minimum or not a number".into());
    }
    if parse_tail_bytes("67108864") != Ok(64 << 20) || parse_sample_seconds("0").is_ok() {
        return Err("--log-tail-bytes or --stall-sample-seconds parsed wrongly".into());
    }
    // Normalization.
    if normalize("t=12 addr 0xdeadBEEF: x") != "t=# addr #: x" {
        return Err(format!(
            "normalize gave {:?}",
            normalize("t=12 addr 0xdeadBEEF: x")
        ));
    }

    // Bounded tail: a 6 MiB log under a 1 MiB budget keeps its size and its last MiB.
    let dir = scratch
        .join("log-tail")
        .join("runs")
        .join("bounded-run")
        .join("verify-logs");
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let log = dir.join("run1.log");
    let mut body = Vec::new();
    for number in 0..(6 << 20) / 32 {
        body.extend_from_slice(format!("line {number:026}\n").as_bytes());
    }
    fs::write(&log, &body).map_err(|error| format!("cannot write {}: {error}", log.display()))?;
    let mut punched = BTreeMap::new();
    let (total, newest) = bound_files(dir.parent().unwrap(), MIN_TAIL_BYTES, &mut punched);
    let after =
        fs::read(&log).map_err(|error| format!("cannot read {}: {error}", log.display()))?;
    let metadata =
        fs::metadata(&log).map_err(|error| format!("cannot stat {}: {error}", log.display()))?;
    let head_cut = punched.get(&log).copied().unwrap_or(0);
    if total != body.len() as u64
        || after.len() != body.len()
        || newest.as_deref() != Some(log.as_path())
    {
        return Err("a bounded log changed its logical size or was not seen".into());
    }
    if head_cut < body.len() as u64 - MIN_TAIL_BYTES - PAGE
        || after[..head_cut as usize].iter().any(|byte| *byte != 0)
        || after[head_cut as usize..] != body[head_cut as usize..]
    {
        return Err(format!(
            "the head was not punched to the last {MIN_TAIL_BYTES} bytes (cut {head_cut})"
        ));
    }
    if metadata.blocks() * 512 > 2 * MIN_TAIL_BYTES {
        return Err(format!(
            "a bounded log still occupies {} bytes",
            metadata.blocks() * 512
        ));
    }
    // Appending continues at the logical offset; a file under 4x the budget is untouched.
    let small = dir.join("small.log");
    fs::write(&small, vec![b'a'; (3 * MIN_TAIL_BYTES) as usize]).map_err(|e| e.to_string())?;
    bound_files(dir.parent().unwrap(), MIN_TAIL_BYTES, &mut punched);
    if punched.contains_key(&small) {
        return Err("a log under four tails was punched".into());
    }

    // New-line counting ignores numbers and repeats.
    let mut seen = BTreeSet::new();
    let repeating = scratch.join("log-tail").join("repeating.log");
    fs::write(&repeating, "tick 1\ntick 2\ntick 3\n").map_err(|e| e.to_string())?;
    if new_lines(&repeating, &mut seen) != 1 || new_lines(&repeating, &mut seen) != 0 {
        return Err("a counter-only change was counted as new lines".into());
    }

    // Classification.
    let series = |cpu_step: u64, log_step: u64, new: u64| -> Vec<Sample> {
        (0..10)
            .map(|index| Sample {
                t: index * 30,
                log_bytes: 1000 + index * log_step,
                cpu_usec: Some(index * cpu_step),
                alive: true,
                new_distinct_lines: new,
            })
            .collect()
    };
    let cases = [
        ("deadlock", series(1_000, 0, 0)),
        ("livelock-silent", series(30_000_000, 0, 0)),
        ("livelock", series(30_000_000, 500, 0)),
        ("still-progressing", series(30_000_000, 500, 3)),
        ("unclear", series(30_000_000, 500, 3)[..2].to_vec()),
    ];
    for (expected, samples) in cases {
        let (class, _) = classify(&samples, 30);
        if class != expected {
            return Err(format!("classified {expected} samples as {class}"));
        }
    }

    // CPU attribution by E2E_RUN_ID, and the record written when the run ends.
    let run_id = format!("log-tail-self-test-{}", std::process::id());
    let runs = scratch.join("log-tail").join("runs");
    fs::create_dir_all(runs.join(&run_id)).map_err(|e| e.to_string())?;
    let mut child = Command::new("sh")
        .args([
            "-c",
            "end=$(($(date +%s) + 2)); while [ $(date +%s) -lt $end ]; do :; done",
        ])
        .env("E2E_RUN_ID", &run_id)
        .spawn()
        .map_err(|error| format!("cannot start the CPU fixture: {error}"))?;
    let options = Options {
        tail_bytes: MIN_TAIL_BYTES,
        interval: Duration::from_millis(400),
    };
    let warden = Warden::start(&scratch.join("log-tail"), options);
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(1200));
    warden.finish();
    let record = read_record(&scratch.join("log-tail"), &run_id)?
        .ok_or("the warden wrote no record for a run that ended")?;
    if record.samples < 2 || record.log_truncated {
        return Err(format!("unexpected record for the CPU fixture: {record:?}"));
    }
    let bounded = read_record(&scratch.join("log-tail"), "bounded-run")?
        .ok_or("the warden wrote no record for a run it bounded")?;
    if bounded.punched_bytes == 0 || !bounded.log_truncated {
        return Err("a run whose logs were punched was not recorded as truncated".into());
    }
    let value = summary_value(&bounded, false);
    if value["class"] != "finished" || value["log_truncated"] != true {
        return Err(format!(
            "summary value of a finished truncated run: {value}"
        ));
    }
    Ok(())
}
