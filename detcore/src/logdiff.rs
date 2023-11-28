/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Everything to do with post-processing hermit/detcore logs.

use core::fmt::Display;
use core::fmt::Formatter;
use core::fmt::Result;
use std::cmp::Ordering;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::Write;
use std::ops::Bound;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use clap;
use clap::Parser;
use lazy_static::lazy_static;
use regex::Regex;
use tempfile::NamedTempFile;

/// Options for calling `log_diff`.
#[derive(Debug, Parser)]
pub struct LogDiffOpts {
    /// Strip numerical information and tmp paths from log lines. This allows comparison even in the
    /// presence of (limited) nondeterminism.
    #[clap(long)]
    pub strip_lines: bool,
    /// Limit the number of differences printed.  Set to 0 for no limit.
    #[clap(long, default_value = "20")]
    pub limit: u64,

    /// Before comparison, filter out lines which contain this substring.
    #[clap(long)]
    pub ignore_lines: Vec<String>,

    /// Show specified number of syscall prior to a divergence point in the diff view. Set to 0 for ommiting the history
    #[clap(long, default_value = "0")]
    pub syscall_history: u64,
    /// Disables colored console output for line diffs
    #[clap(long)]
    pub no_color: bool,

    /// Do not consider "COMMIT" messages for deterministic check
    #[clap(long)]
    pub skip_commit: bool,

    /// Do not consider "DETLOG" messages for deterministic check
    #[clap(long)]
    pub skip_detlog: bool,

    /// Use git diff instead of the internal, basic log comparison.
    #[clap(long)]
    pub git_diff: bool,

    /// In case --skip-detlog=false this parameter further filters which
    /// "DETLOG" messages will be included for determnistic check
    #[clap(long, default_values = &["syscall", "syscallresult", "other"])]
    pub include_detlogs: Vec<DetLogFilter>,
}

impl LogDiffOpts {
    fn is_skip(&self, filter: DetLogFilter) -> bool {
        !self.include_detlogs.contains(&filter)
    }

    fn skip_detlog(&self, entry: &str) -> bool {
        if self.skip_detlog {
            return true;
        }

        if is_detlog_syscall(entry) && self.is_skip(DetLogFilter::Syscall) {
            return true;
        }
        if is_detlog_syscall_result(entry) && self.is_skip(DetLogFilter::SyscallResult) {
            return true;
        }

        if !is_detlog_syscall(entry)
            && !is_detlog_syscall_result(entry)
            && self.is_skip(DetLogFilter::Other)
        {
            return true;
        }

        false
    }

    fn filter_deterministic<'a>(&self, v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
        v.iter()
            .filter_map(|(i, s)| {
                if (is_detlog(s) && !self.skip_detlog(s)) || (is_commit(s) && !self.skip_commit) {
                    Some((*i, *s))
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Indicates which DETLOG entries to be used for log-diff comparison
#[derive(Debug, PartialEq, Eq)]
pub enum DetLogFilter {
    ///the start of syscall will be used for logdiff
    Syscall,
    ///the syscall result  will be used for logdiff
    SyscallResult,
    ///all other unspecified DETLOG entries will be used for logdiff
    Other,
}

impl FromStr for DetLogFilter {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "syscall" => Ok(DetLogFilter::Syscall),
            "syscallresult" => Ok(DetLogFilter::SyscallResult),
            "other" => Ok(DetLogFilter::Other),
            _ => Err(anyhow::Error::msg(format!(
                "unknown value {} for DetLogFilter",
                s
            ))),
        }
    }
}

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
impl Default for LogDiffOpts {
    fn default() -> Self {
        let v: Vec<String> = vec![];
        LogDiffOpts::from_iter(v.iter())
    }
}

/// In fully-deterministic modes, many log lines should be fully determinstic across runs.
/// But as that is a work-in-progress, this utility strips known-nondeterministic
/// information from logs.
///
/// INPUT: `full_container` bool flag, which specifies whether we should expect that the
/// log message was generated from a full run of hermit/detcore against a standalone
/// binary, with all the setup that entails. N.B.: this should be *false* for `spawn_fn_*`
/// variants which fork from another process, not having a true process tree of their own.
///
/// Example input/output:
///   `Input:  COMMIT turn 3, dettid 231635 using resources Resources { tid: DetPid { inner: 231635 }, resources: {Path("/proc/231635/fd/1"): W} }`
///   `Output: COMMIT turn <NUM>, dettid <NUM> using resources Resources { tid: DetPid { inner: <NUM> }, resources: {Path("/proc/<pid>/fd/<num>"): W} }`
///
/// As you can see this is overkill and smarter strategies would be possible. For example,
/// ones that remember and post-facto-determinize certain identifiers.
pub fn strip_log_entry(log: &str) -> String {
    lazy_static! {
        // Memory addresses, like 0x7fcfb7e7d450
        //
        // TODO: use a debug allocator that increases only, never reusing. Also, consider
        // post-facto processing all of these into new virtual addresses based on the order they're seen.
        static ref RE0: Regex = Regex::new(r"\b0[xX][A-Fa-f0-9]+\b").unwrap();

        // The basic reason for treating spawn_fn style tests differently is that using
        // detcore for these forked function calls is really a *partial* version of detcore,
        // not the full and proper setup.  In contrast, for all command tests, and for `hermit
        // run` itself, the full contents of the COMMIT line should be deterministic and this
        // hack should not be needed.
        static ref RE1: Regex = Regex::new(r"\b[\d]+\b").unwrap(); // This one is terrible overkill.

        // TODO: only strip this information if the config specified to the host /tmp through.
        // Otherwise we can determinize /tmp access fully.
        static ref RE2: Regex = Regex::new(r#"/tmp/.*""#).unwrap();

        // TODO: only strip this one if we're allowing through the host /proc or failing to determinize tids/pids:
        static ref RE3: Regex = Regex::new(r"/proc/[\d]+/").unwrap();

        // TODO: only strip this if we're running a library-based test where we can't
        // guarantee the starting state of the allocator/etc.
        static ref RE4: Regex = Regex::new(r"\b[\d][\d_.]*s\b").unwrap();
    }
    let log = RE4.replace_all(log, "<NANOSECONDS>");
    let log = RE3.replace_all(&log, "/proc/<PID>/");
    let log = RE0.replace_all(&log, "<ADDR>");
    let log = RE1.replace_all(&log, "<NUM>");
    let log = RE2.replace_all(&log, "/tmp/<somewhere>");
    String::from(log)
}

/// Separate a full, continuous log into discrete (possibly-multiline) log messages,
/// stripping off the timestamps in the process.  Return lines tagged with their
/// index number.
fn extract_log_messages(contents: &str) -> Vec<(usize, &str)> {
    let ts =
        Regex::new(r"((Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d\d \d\d:\d\d:\d\d\.\d+|\d+-\d\d-\d\dT\d\d:\d\d:\d\d.\d+Z) +")
            .unwrap();
    let tag = Regex::new("^(ERROR|WARN|INFO|DEBUG|TRACE) ").unwrap();
    let iter = ts
        .split(contents) // Not aware of a streaming version of this RE split operation.
        .enumerate()
        .map(|(i, s)| (i, s.trim()))
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| {
            // Only let through lines that start with one of the expected tags:
            if !tag.is_match(s) {
                panic!("Log line without expected tag: {}", s);
            } else {
                (i, s)
            }
        });
    iter.collect()
}

fn is_info(line: &str) -> bool {
    line.starts_with("INFO ")
}

fn is_commit(line: &str) -> bool {
    line.contains(" COMMIT ")
}

fn is_detlog(line: &str) -> bool {
    line.contains(" DETLOG ")
}

fn is_detcore(line: &str) -> bool {
    lazy_static! {
        static ref PREFIX: Regex = Regex::new("^(ERROR|WARN|INFO|DEBUG|TRACE).* detcore:").unwrap();
    }
    PREFIX.is_match(line)
}

fn is_detlog_syscall(line: &str) -> bool {
    is_detlog(line) && line.contains("[syscall]")
}

fn is_detlog_syscall_result(line: &str) -> bool {
    is_detlog_syscall(line) && line.contains("finish syscall")
}

// TODO:
// Append together a sequence of messages while truncating if there are too many.
fn _truncate_messages(_v: &[&str]) -> String {
    unimplemented!()
}

fn filter_infos<'a>(v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
    v.iter().filter(|(_i, s)| is_info(s)).copied().collect()
}

fn filter_detcore<'a>(v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
    v.iter().filter(|(_, s)| is_detcore(s)).copied().collect()
}

fn filter_ignored<'a>(lines: Vec<(usize, &'a str)>, omits: &Vec<String>) -> Vec<(usize, &'a str)> {
    lines
        .into_iter()
        .filter(|(_ix, ln)| {
            let mut keep = true;
            for omit in omits {
                if ln.contains(omit) {
                    keep = false
                }
            }
            keep
        })
        .collect()
}

fn collect_syscalls<'a>(v: &[(usize, &'a str)]) -> BTreeMap<Reverse<usize>, &'a str> {
    // here we order syscalls by their original indices in logfile.
    // we want a non-increasing order to be able to efficiently take(#number_of_recent_calls) using range_search
    v.iter()
        .filter_map(|(i, s)| {
            if is_detlog_syscall_result(s) {
                Some((Reverse(*i), *s))
            } else {
                None
            }
        })
        .collect()
}

/// A comparison of two strings.
///
/// Displays comparison result without any formatting
pub struct Comparison<'a> {
    left: &'a str,
    right: &'a str,
    no_color: bool,
}
impl<'a> Comparison<'a> {
    /// Store two values to be compared in future.
    ///
    /// Expensive diffing is deferred until calling `Debug::fmt`.
    pub fn new(no_color: bool, left: &'a str, right: &'a str) -> Comparison<'a> {
        Comparison {
            left,
            right,
            no_color,
        }
    }
}
impl<'a> Display for Comparison<'a> {
    fn fmt(&self, f: &mut Formatter) -> Result {
        if self.no_color {
            writeln!(f, "Diff < left / right > :")?;
            writeln!(f, "<\"{}\"", self.left)?;
            writeln!(f, ">\"{}\"", self.right)
        } else {
            pretty_assertions::Comparison::new(&self.left, &self.right).fmt(f)
        }
    }
}

/// Returns `true` if a difference is found.
///
/// We could use an existing diff library on the entire log, but this provides us more
/// control over how to present the (stripped/unstripped) differences, and to focus on the
/// per-line divergence(s), and potentially focus on the first point of divergence.
//
// Future TODO:
//  - report bulk differences, e.g. leftover lines, but with truncation
//  - report only the first K per-line differences
//  - detect reorderings and/or switch to larger differences for consecutive multi-line mismatches
fn diff_vecs(
    which: &str,
    v1: &[(usize, &str)],
    v2: &[(usize, &str)],
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    syscalls: &BTreeMap<Reverse<usize>, &str>,
) -> std::io::Result<bool> {
    writeln!(w, "  Comparing {} messages...\n", which)?;
    if v1.is_empty() && v2.is_empty() {
        Ok(false)
    } else {
        let mut diff_count = 0;
        for (_ix, ((oix, ox), (_, oy))) in v1.iter().zip(v2.iter()).enumerate() {
            let (x, y) = if opts.strip_lines {
                (strip_log_entry(ox), strip_log_entry(oy))
            } else {
                (ox.to_string(), oy.to_string())
            };
            if x != y {
                write!(
                    w,
                    "({}) Mismatch in log entries, line {}: {}",
                    which,
                    oix,
                    Comparison::new(opts.no_color, &x, &y)
                )?;
                if opts.strip_lines {
                    write!(
                        w,
                        "({}) Original entries, before stripping {}: {}",
                        which,
                        oix,
                        Comparison::new(opts.no_color, ox, oy)
                    )?;
                }
                if opts.syscall_history > 0 {
                    let mut syscall_history: Vec<_> = syscalls
                        .range((Bound::Excluded(Reverse(*oix)), Bound::Unbounded))
                        .take(5)
                        .collect();
                    syscall_history.reverse();

                    writeln!(w, "Recent syscalls: ")?;
                    if !syscall_history.is_empty() {
                        for (_, s) in syscall_history {
                            writeln!(w, "{}", s)?;
                        }
                        writeln!(w)?;
                    } else {
                        writeln!(w, "None\n")?;
                    }
                }

                diff_count += 1;
                if diff_count > opts.limit && opts.limit != 0 {
                    writeln!(
                        w,
                        "More than {} differences, eliding the rest...",
                        opts.limit
                    )?;
                    break;
                }
            }
        }

        match v1.len().cmp(&v2.len()) {
            Ordering::Less => {
                writeln!(
                    w,
                    "second log contains {} extra lines not matched in log two. Displaying up to 10 of those:",
                    v2.len() - v1.len()
                )?;
                diff_count += 1;
                let start = v2.len() - std::cmp::min(10, v2.len() - v1.len());
                for ln in v2[start..v2.len()].iter() {
                    writeln!(w, "{}", ln.1)?;
                }
            }
            Ordering::Greater => {
                writeln!(
                    w,
                    "First log contains {} extra lines not matched in log two.  Displaying up to 10 of those:",
                    v1.len() - v2.len()
                )?;
                diff_count += 1;
                let start = v1.len() - std::cmp::min(10, v1.len() - v2.len());
                for ln in v1[start..v1.len()].iter() {
                    writeln!(w, "{}", ln.1)?;
                }
            }
            Ordering::Equal => {}
        }

        Ok(diff_count > 0)
    }
}

fn git_diff(
    which: &str,
    v1: &[(usize, &str)],
    v2: &[(usize, &str)],
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    syscalls: &BTreeMap<Reverse<usize>, &str>,
) -> std::io::Result<bool> {
    writeln!(w, "  Comparing {} messages...\n", which)?;

    let mut file1 = NamedTempFile::new()?;
    let mut file2 = NamedTempFile::new()?;

    for (_, ln) in v1 {
        writeln!(file1, "{}", ln)?;
    }
    for (_, ln) in v2 {
        writeln!(file2, "{}", ln)?;
    }

    // git diff --color --color-words -w
    match Command::new("git")
        .args(["diff", "--color", "--color-words", "-w"])
        .arg(file1.path())
        .arg(file2.path())
        .status()
    {
        Ok(code) => Ok(!code.success()),
        Err(err) => {
            eprintln!("Error launching git, falling back to basic diff: {}", err);
            diff_vecs(which, v1, v2, opts, w, syscalls)
        }
    }
}

/// Process log messages from two files.  Log messages look like this:
///     "Apr 09 06:08:03.100  INFO detcore: [detcore, dtid 2]  finish syscall: close(2) = Ok(0)"
///
/// With some complexities:
///  * Some entries are multi-line (contain newlines).
///  * Some stripping of nondeterministic information is needed for direct comparability.
///  * Certain lines are intended to be deterministic/comparable, in their contents,
///    and others in their *presence* but not their details.
//
// TODO: we should replace this with a diff algorithm that can handle insertions while maintaining
// alignment. There's also no reason we can't output the stripped relevant lines and use a separate
// diff tool.
pub fn log_diff(file_a: &Path, file_b: &Path, opts: &LogDiffOpts) -> bool {
    // For now the log-diff mode reads both logs fully into memory. This could be
    // modified in the future for a streaming solution, at least for scrolling through
    // the identical prefixes of very large logs.
    let vec_a = std::fs::read(file_a).expect("Could not open first input file.");
    let vec_b = std::fs::read(file_b).expect("Could not open second input file.");
    let str_a = String::from_utf8_lossy(&vec_a);
    let str_b = String::from_utf8_lossy(&vec_b);
    log_diff_from_strs(str_a, str_b, opts, &mut std::io::stderr())
        .expect("should write succesfully")
}

fn log_diff_from_strs(
    file_a_str: impl AsRef<str>,
    file_b_str: impl AsRef<str>,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<bool> {
    let vec_a = extract_log_messages(file_a_str.as_ref());
    let vec_b = extract_log_messages(file_b_str.as_ref());

    let vec_a = filter_ignored(vec_a, &opts.ignore_lines);
    let vec_b = filter_ignored(vec_b, &opts.ignore_lines);

    writeln!(
        w,
        "Logs contain {} | {} messages total",
        vec_a.len(),
        vec_b.len(),
    )?;

    let vec_a = filter_detcore(&vec_a);
    let vec_b = filter_detcore(&vec_b);
    let infos_a = filter_infos(&vec_a);
    let infos_b = filter_infos(&vec_b);
    let detlogs_a = opts.filter_deterministic(&vec_a);
    let detlogs_b = opts.filter_deterministic(&vec_b);
    let syscalls = collect_syscalls(&vec_a);
    writeln!(
        w,
        "Logs contain {} | {} detcore-specific messages",
        vec_a.len(),
        vec_b.len(),
    )?;
    writeln!(
        w,
        "Logs contain {} | {} INFO messages",
        infos_a.len(),
        infos_b.len(),
    )?;
    writeln!(
        w,
        "Logs contain {} | {} DETLOG & scheduler COMMIT messages",
        detlogs_a.len(),
        detlogs_b.len(),
    )?;

    if opts.strip_lines {
        writeln!(w, "Stripping entries of numerical data before comparison..")?;
    }

    let mut diff_found = false;

    if opts.git_diff {
        diff_found |= git_diff("DETLOGs", &detlogs_a, &detlogs_b, opts, w, &syscalls)?;
    } else {
        diff_found |= diff_vecs("DETLOGs", &detlogs_a, &detlogs_b, opts, w, &syscalls)?;
    }

    // The TRACE level lines will generally include reorderings from racing threads.
    // So not doing any comparisons here for now:
    // diff_found |= diff_vecs("ALL", &vec_a, &vec_b);

    if diff_found {
        writeln!(w, "Done processing logs, differences found.")?;
    } else {
        writeln!(w, "Done processing logs, no substantive differences found.")?;
    }
    Ok(diff_found)
}

#[cfg(test)]
mod test {
    use pretty_assertions::assert_eq;

    use crate::logdiff::DetLogFilter;

    #[test]
    fn test_compare_with_no_color() {
        let str1 = "test1";
        let str2 = "test2";

        assert_eq!(
            format!("{}", &super::Comparison::new(true, str1, str2))
                .split('\n')
                .collect::<Vec<&str>>(),
            ["Diff < left / right > :", "<\"test1\"", ">\"test2\"", "",]
        );
    }

    #[test]
    fn test_compare_with_color() {
        let str1 = "test1";
        let str2 = "test2";

        assert_eq!(
            format!("{}", &super::Comparison::new(false, str1, str2))
                .split('\n')
                .collect::<Vec<&str>>(),
            [
                "\u{1b}[1mDiff\u{1b}[0m \u{1b}[31m< left\u{1b}[0m / \u{1b}[32mright >\u{1b}[0m :",
                "\u{1b}[31m<\"test\u{1b}[0m\u{1b}[1;48;5;52;31m1\u{1b}[0m\u{1b}[31m\"\u{1b}[0m",
                "\u{1b}[32m>\"test\u{1b}[0m\u{1b}[1;48;5;22;32m2\u{1b}[0m\u{1b}[32m\"\u{1b}[0m",
                "",
            ]
        );
    }

    #[test]
    fn test_log_diff_with_color() -> std::io::Result<()> {
        let str1 = "INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #11: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)";
        let str2 = "INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #15: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)";
        let mut result = Vec::<u8>::new();

        super::log_diff_from_strs(
            str1,
            str2,
            &super::LogDiffOpts {
                limit: 1,
                strip_lines: false,
                syscall_history: 5,
                no_color: false,
                skip_commit: false,
                skip_detlog: false,
                git_diff: false,
                ignore_lines: Vec::new(),
                include_detlogs: vec![
                    DetLogFilter::Syscall,
                    DetLogFilter::SyscallResult,
                    DetLogFilter::Other,
                ],
            },
            &mut result,
        )?;

        assert_eq!(
            String::from_utf8_lossy(&result)
                .as_ref()
                .split('\n')
                .collect::<Vec<&str>>(),
            vec![
                "Logs contain 1 | 1 messages total",
                "Logs contain 1 | 1 detcore-specific messages",
                "Logs contain 1 | 1 INFO messages",
                "Logs contain 1 | 1 DETLOG & scheduler COMMIT messages",
                "  Comparing DETLOGs messages...",
                "",
                "(DETLOGs) Mismatch in log entries, line 0: \u{1b}[1mDiff\u{1b}[0m \u{1b}[31m< left\u{1b}[0m / \u{1b}[32mright >\u{1b}[0m :",
                "\u{1b}[31m<\"INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #1\u{1b}[0m\u{1b}[1;48;5;52;31m1\u{1b}[0m\u{1b}[31m: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)\"\u{1b}[0m",
                "\u{1b}[32m>\"INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #1\u{1b}[0m\u{1b}[1;48;5;22;32m5\u{1b}[0m\u{1b}[32m: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)\"\u{1b}[0m",
                "Recent syscalls: ",
                "None",
                "",
                "Done processing logs, differences found.",
                "",
            ]
        );

        Ok(())
    }

    #[test]
    fn test_log_diff_syscalls() -> std::io::Result<()> {
        let log_file_a = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:48.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 1
2022-09-06T14:15:48.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 7984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:48.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 2
2022-09-06T14:15:48.904049Z  INFO detcore: COMMIT 2
2022-09-06T14:15:48.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;

        let log_file_b = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:48.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 1
2022-09-06T14:15:47.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 1984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:47.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 3
2022-09-06T14:15:47.904049Z  INFO detcore: COMMIT 1
2022-09-06T14:15:47.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;
        let mut result = Vec::<u8>::new();

        super::log_diff_from_strs(
            log_file_a,
            log_file_b,
            &super::LogDiffOpts {
                limit: 1,
                strip_lines: false,
                syscall_history: 5,
                no_color: true,
                skip_commit: false,
                skip_detlog: false,
                git_diff: false,
                ignore_lines: Vec::new(),
                include_detlogs: vec![
                    DetLogFilter::Syscall,
                    DetLogFilter::SyscallResult,
                    DetLogFilter::Other,
                ],
            },
            &mut result,
        )?;

        assert_eq!(
            String::from_utf8_lossy(&result)
                .as_ref()
                .split('\n')
                .collect::<Vec<&str>>(),
            vec![
                "Logs contain 6 | 6 messages total",
                "Logs contain 6 | 6 detcore-specific messages",
                "Logs contain 6 | 6 INFO messages",
                "Logs contain 5 | 5 DETLOG & scheduler COMMIT messages",
                "  Comparing DETLOGs messages...",
                "",
                "(DETLOGs) Mismatch in log entries, line 3: Diff < left / right > :",
                "<\"INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 7984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe\"",
                ">\"INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 1984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe\"",
                "Recent syscalls: ",
                "INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 1",
                "",
                "(DETLOGs) Mismatch in log entries, line 4: Diff < left / right > :",
                "<\"INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 2\"",
                ">\"INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 3\"",
                "Recent syscalls: ",
                "INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall: write(1, 0x6022a0, 70) = 1",
                "",
                "More than 1 differences, eliding the rest...",
                "Done processing logs, differences found.",
                ""
            ]
        );
        Ok(())
    }

    #[test]
    fn test_log_diff_compares_detlog() -> std::io::Result<()> {
        let log_file_a = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:48.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 7984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:48.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: write(1, 0x6022a0, 70) = ?
2022-09-06T14:15:48.904049Z  INFO detcore: COMMIT 2
2022-09-06T14:15:48.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;

        let log_file_b = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:47.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 1984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:47.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: write(1, 0x6022a0, 70) = ?
2022-09-06T14:15:47.904049Z  INFO detcore: COMMIT 1
2022-09-06T14:15:47.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;
        let mut result = Vec::<u8>::new();

        let log_options = super::LogDiffOpts {
            no_color: true,
            git_diff: false,
            ..Default::default()
        };
        super::log_diff_from_strs(log_file_a, log_file_b, &log_options, &mut result)?;

        assert_eq!(
            String::from_utf8_lossy(&result)
                .as_ref()
                .split('\n')
                .collect::<Vec<&str>>(),
            vec![
                "Logs contain 5 | 5 messages total",
                "Logs contain 5 | 5 detcore-specific messages",
                "Logs contain 5 | 5 INFO messages",
                "Logs contain 4 | 4 DETLOG & scheduler COMMIT messages",
                "  Comparing DETLOGs messages...",
                "",
                "(DETLOGs) Mismatch in log entries, line 2: Diff < left / right > :",
                "<\"INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 7984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe\"",
                ">\"INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 1984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe\"",
                "(DETLOGs) Mismatch in log entries, line 4: Diff < left / right > :",
                "<\"INFO detcore: COMMIT 2\"",
                ">\"INFO detcore: COMMIT 1\"",
                "Done processing logs, differences found.",
                ""
            ]
        );
        Ok(())
    }

    #[test]
    fn test_filter_deterministic() {
        let opts = super::LogDiffOpts {
            include_detlogs: vec![
                DetLogFilter::Syscall,
                DetLogFilter::SyscallResult,
                DetLogFilter::Other,
            ],
            ..Default::default()
        };

        let v = opts.filter_deterministic(
            &[
                (
                    1,
                    "INFO detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                (
                    2,
                    "INFO DETLOG detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ],
        );

        assert_eq!(
            v,
            vec![
                (
                    2,
                    "INFO DETLOG detcore: registers [dtid 3]. user_regs_struct { r15..."
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ]
        );
    }

    #[test]
    fn test_filter_deterministic_with_filter() {
        let opts = super::LogDiffOpts {
            include_detlogs: vec![DetLogFilter::Syscall],
            skip_commit: true,
            ..Default::default()
        };

        let v = opts.filter_deterministic(
            &[
                (
                    1,
                    "INFO detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                (
                    2,
                    "INFO DETLOG detcore:[syscall] syscall 1",
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ],
        );
        assert_eq!(v, vec![(2, "INFO DETLOG detcore:[syscall] syscall 1")]);
    }

    #[test]
    fn test_filter_infos() {
        let v = super::filter_infos(&[
            (
                0,
                "DEBUG detcore::scheduler: [sched-step3] advancing committed_time from 946684799165300000 to 946684799205300000",
            ),
            (
                1,
                "INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, ...",
            ),
        ]);
        assert_eq!(
            v,
            vec![(
                1,
                "INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, ..."
            )]
        );
    }

    #[test]
    fn test_extract_log_messages() {
        let s = "
Jan 09 06:08:03.100  INFO detcore: [detcore, dtid 2]  finish syscall: close(2) = Ok(0)
Feb 09 06:49:17.742 DEBUG detcore::scheduler: [sched-step3] advancing committed_time from 946684799165300000 to 946684799205300000
Apr 09 06:49:17.742  INFO detcore::scheduler: [scheduler] >>>>>>>

 COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000
Jan 09 06:49:03.100  INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, r14: 0, r13: 1, r12: 946684799000118840, rbp: 140737488344736, rbx: 0, r11: 518, r10: 140737488342434, r9: 0, r8: 1, rax: 0, rcx: 0, rdx: 2, rsi: 0, rdi: 140737354052880, orig_rax: 18446744073709551615, rip: 140737351875567, cs: 51, eflags: 66118, rsp: 140737488344064, ss: 43, fs_base: 0, gs_base: 0, ds: 0, es: 0, fs: 0, gs: 0 }
Jun 09 06:49:17.742 TRACE detcore::scheduler: [scheduler] Guest unblocked (<ivar 0x7fa26067a150 Go>); clear ivars for the next turn on dettid 2
";

        let v = super::extract_log_messages(s);
        eprintln!("Split into {} log messages", v.len());
        for x in &v {
            eprintln!("{:?}", x);
        }
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn test_strip_log() {
        assert_eq!(super::strip_log_entry("800.709_180s"), "<NANOSECONDS>");
        assert_eq!(
            super::strip_log_entry(
                "COMMIT turn 66, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946_684_800.709_180_000s"
            ),
            "COMMIT turn <NUM>, dettid <NUM> using resources {Path(\"/proc/<PID>/fd/<NUM>\"): W} at time <NANOSECONDS>"
        );
    }
}
