#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Analyze Hermit/Detcore scheduler *timeslice* structure from a hermit log.
//!
//! Feed it the stderr/log of a `hermit run --log info` (or debug/trace) run:
//!
//!     ./scripts/log_timeslice.rs < /tmp/qemu-hermit.log
//!     hermit run --log info --strict -- ./guest 2>&1 | ./scripts/log_timeslice.rs
//!
//! It reconstructs each timeslice (the quantum a thread runs before it is
//! preempted / yields) and reports, per slice: owning dtid, wall-clock
//! duration, *virtual* (committed logical) time advanced, syscall count, COMMIT
//! turns, retired-conditional-branch (rcb) work, and raw log lines. It then
//! prints the turn-taking pattern and flags anomalies — the case this tool was
//! built for: virtual time barely advancing across hundreds of syscalls, or a
//! single timeslice dominating wall time.
//!
//! Virtual time and rcbs are GLOBAL cumulative counters in the log, so each
//! slice's advance is measured as (value at this slice's end) - (value at the
//! previous slice's end); this correctly attributes advance to slices that emit
//! only one COMMIT.
//!
//! Markers parsed (all emitted at --log info and below):
//!   * `[detcore, dtid D] ending timeslice TN. X syscalls and Y signals ...`
//!   * ` COMMIT turn N, dettid D ... on previously committed <VT>s`
//!   * `[dtid D] inbound rdtsc, new logical time: DetTime { ... rcbs: R, ... }`
//!   * `DETLOG [syscall]... inbound syscall:`

use std::io::{self, Read};

#[derive(Default, Clone)]
struct Slice {
    dtid: u64,
    tnum: u64,
    partial: bool, // truncated final slice (no "ending timeslice" line)
    syscalls: u64,
    signals: u64,
    commits: u64,
    lines: u64,
    inbound_syscalls: u64,
    wall_dur_ns: i128,
    virt_dur_ns: i128,
    rcb_delta: u64,
}

/// Parse the leading ISO timestamp (`2026-07-23T22:51:37.485365Z`) to
/// nanoseconds-within-the-day. None for lines with no wall timestamp (e.g. raw
/// DETLOG `COMMIT` lines).
fn parse_wall_ns(line: &str) -> Option<i128> {
    let t = line.find('T')?;
    if t < 4 || !line.as_bytes()[t - 1].is_ascii_digit() {
        return None;
    }
    let rest = &line[t + 1..];
    let z = rest.find('Z')?;
    let hms = &rest[..z];
    let mut it = hms.splitn(3, ':');
    let h: i128 = it.next()?.parse().ok()?;
    let m: i128 = it.next()?.parse().ok()?;
    let sfrac = it.next()?;
    let (secs, frac) = match sfrac.split_once('.') {
        Some((s, f)) => (s.parse::<i128>().ok()?, f),
        None => (sfrac.parse::<i128>().ok()?, ""),
    };
    Some(((h * 3600 + m * 60 + secs) * 1_000_000_000) + frac_to_ns(frac))
}

/// Parse a virtual-time literal like `1_640_995_199.000_500_000s` into ns.
fn parse_virt_ns(tok: &str) -> Option<i128> {
    let clean: String = tok.chars().filter(|c| *c != '_').collect();
    let clean = clean.trim_end_matches('s');
    let (secs, frac) = clean.split_once('.').unwrap_or((clean, ""));
    Some(secs.parse::<i128>().ok()? * 1_000_000_000 + frac_to_ns(frac))
}

/// Normalize a fractional-seconds string to nanoseconds (pad/truncate to 9).
fn frac_to_ns(frac: &str) -> i128 {
    let mut ns: i128 = 0;
    for (i, c) in frac.bytes().enumerate() {
        if i >= 9 || !c.is_ascii_digit() {
            break;
        }
        ns = ns * 10 + (c - b'0') as i128;
    }
    for _ in frac.len().min(9)..9 {
        ns *= 10;
    }
    ns
}

/// Extract the integer immediately following `needle` in `line`.
fn num_after(line: &str, needle: &str) -> Option<u64> {
    let i = line.find(needle)? + needle.len();
    let rest = &line[i..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn fmt_ms(ns: i128) -> String {
    format!("{:.3}", ns as f64 / 1_000_000.0)
}

fn label(s: &Slice) -> String {
    if s.partial {
        "tail".to_string()
    } else {
        format!("T{}", s.tnum)
    }
}

fn main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("read stdin");

    let mut slices: Vec<Slice> = Vec::new();
    let mut cur = Slice::default();
    let mut have_cur = false;

    let mut first_wall: Option<i128> = None;
    let mut last_wall: Option<i128> = None;
    let mut prev_end_wall: Option<i128> = None;

    // Global cumulative trackers; per-slice advance is a delta of these.
    let mut last_virt: Option<i128> = None;
    let mut prev_end_virt: Option<i128> = None;
    let mut last_rcb: Option<u64> = None;
    let mut prev_end_rcb: Option<u64> = None;

    for line in input.lines() {
        if let Some(w) = parse_wall_ns(line) {
            first_wall.get_or_insert(w);
            last_wall = Some(w);
        }
        if have_cur {
            cur.lines += 1;
        }

        if let Some(cpos) = line.find("COMMIT turn ") {
            have_cur = true;
            cur.commits += 1;
            if cur.dtid == 0 {
                if let Some(d) = num_after(&line[cpos..], "dettid ") {
                    cur.dtid = d;
                }
            }
            if let Some(vpos) = line.find("previously committed ") {
                let tok = line[vpos + "previously committed ".len()..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if let Some(v) = parse_virt_ns(tok) {
                    last_virt = Some(v);
                    // Baseline at the first sample so slice 0 measures its own advance.
                    prev_end_virt.get_or_insert(v);
                }
            }
            continue;
        }

        if line.contains("inbound rdtsc") {
            have_cur = true;
            if let Some(r) = num_after(line, "rcbs: ") {
                last_rcb = Some(r);
                prev_end_rcb.get_or_insert(r);
            }
            continue;
        }

        if line.contains("inbound syscall:") {
            have_cur = true;
            cur.inbound_syscalls += 1;
            continue;
        }

        if let Some(epos) = line.find("ending timeslice T") {
            let after = &line[epos + "ending timeslice T".len()..];
            cur.tnum = after
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("")
                .parse()
                .unwrap_or(0);
            if let Some(p) = line.find("dtid ") {
                if let Some(d) = num_after(&line[p..], "dtid ") {
                    cur.dtid = d; // ending line is authoritative for owner
                }
            }
            cur.syscalls = after
                .split_once(". ")
                .and_then(|(_, r)| r.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(cur.inbound_syscalls);
            cur.signals = line
                .find(" and ")
                .and_then(|p| num_after(&line[p..], " and "))
                .unwrap_or(0);

            let end_wall = parse_wall_ns(line).or(last_wall).unwrap_or(0);
            let start = prev_end_wall.or(first_wall).unwrap_or(end_wall);
            cur.wall_dur_ns = (end_wall - start).max(0);
            prev_end_wall = Some(end_wall);

            cur.virt_dur_ns = delta_virt(last_virt, &mut prev_end_virt);
            cur.rcb_delta = delta_rcb(last_rcb, &mut prev_end_rcb);

            slices.push(std::mem::take(&mut cur));
            have_cur = false;
            continue;
        }
    }
    // Trailing partial slice (log truncated mid-timeslice, e.g. QEMU timeout).
    if have_cur && (cur.commits > 0 || cur.inbound_syscalls > 0) {
        cur.partial = true;
        cur.syscalls = cur.inbound_syscalls;
        let end_wall = last_wall.unwrap_or(0);
        let start = prev_end_wall.or(first_wall).unwrap_or(end_wall);
        cur.wall_dur_ns = (end_wall - start).max(0);
        cur.virt_dur_ns = delta_virt(last_virt, &mut prev_end_virt);
        cur.rcb_delta = delta_rcb(last_rcb, &mut prev_end_rcb);
        slices.push(cur);
    }

    if slices.is_empty() {
        eprintln!(
            "no timeslices found. Expected 'ending timeslice T..' and/or ' COMMIT turn ..' \
             lines — run hermit with --log info (or debug/trace)."
        );
        std::process::exit(2);
    }

    report(&slices, first_wall, last_wall);
}

fn delta_virt(last: Option<i128>, prev_end: &mut Option<i128>) -> i128 {
    let d = match (last, *prev_end) {
        (Some(l), Some(p)) => (l - p).max(0),
        _ => 0,
    };
    if last.is_some() {
        *prev_end = last;
    }
    d
}

fn delta_rcb(last: Option<u64>, prev_end: &mut Option<u64>) -> u64 {
    let d = match (last, *prev_end) {
        (Some(l), Some(p)) => l.saturating_sub(p),
        _ => 0,
    };
    if last.is_some() {
        *prev_end = last;
    }
    d
}

fn median(mut v: Vec<i128>) -> i128 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn report(slices: &[Slice], first_wall: Option<i128>, last_wall: Option<i128>) {
    let total_wall = match (first_wall, last_wall) {
        (Some(a), Some(b)) => b - a,
        _ => 0,
    };
    let total_virt: i128 = slices.iter().map(|s| s.virt_dur_ns).sum();
    let total_sys: u64 = slices.iter().map(|s| s.syscalls).sum();
    let total_commits: u64 = slices.iter().map(|s| s.commits).sum();
    let total_rcb: u64 = slices.iter().map(|s| s.rcb_delta).sum();
    let mut dtids: Vec<u64> = slices.iter().map(|s| s.dtid).collect();
    dtids.sort_unstable();
    dtids.dedup();

    println!("=== Hermit timeslice analysis ===");
    println!(
        "timeslices: {}    dtids: {:?}    COMMIT turns: {}    syscalls: {}    rcbs: {}",
        slices.len(),
        dtids,
        total_commits,
        total_sys,
        total_rcb
    );
    println!(
        "wall span: {} ms    virtual advanced: {} ms    virt/wall: {:.4}    virt/syscall: {} us",
        fmt_ms(total_wall),
        fmt_ms(total_virt),
        if total_wall > 0 {
            total_virt as f64 / total_wall as f64
        } else {
            0.0
        },
        if total_sys > 0 {
            format!("{:.2}", (total_virt as f64 / 1000.0) / total_sys as f64)
        } else {
            "n/a".into()
        }
    );
    println!();

    let wall_med = median(slices.iter().map(|s| s.wall_dur_ns).collect());
    let long_wall = (wall_med * 5).max(1_000_000); // >5x median wall, min 1ms
    let big_jump = 20_000_000i128; // 20ms virtual advance in one slice

    println!(
        "{:>4} {:>4} {:>6} {:>10} {:>10} {:>8} {:>7} {:>10} {:>6}  flags",
        "idx", "dtid", "slice", "wall_ms", "virt_ms", "syscall", "commit", "rcbs", "lines"
    );
    for (i, s) in slices.iter().enumerate() {
        let mut flags = String::new();
        if s.partial {
            flags.push_str("PARTIAL ");
        }
        if s.wall_dur_ns > long_wall {
            flags.push_str("LONG-WALL ");
        }
        // idx 0 has no prior slice to measure virtual/rcb advance against.
        if s.syscalls >= 5 && s.virt_dur_ns == 0 {
            flags.push_str("VT0 ");
        }
        if s.virt_dur_ns >= big_jump {
            flags.push_str("VT-JUMP ");
        }
        println!(
            "{:>4} {:>4} {:>6} {:>10} {:>10} {:>8} {:>7} {:>10} {:>6}  {}",
            i,
            s.dtid,
            label(s),
            fmt_ms(s.wall_dur_ns),
            fmt_ms(s.virt_dur_ns),
            s.syscalls,
            s.commits,
            s.rcb_delta,
            s.lines,
            flags.trim_end()
        );
    }
    println!();

    // Turn-taking pattern (run-length compressed dtid sequence).
    print!("turn-taking (dtid order): ");
    let mut it = slices.iter().map(|s| s.dtid);
    if let Some(mut prev) = it.next() {
        let mut run = 1usize;
        let mut parts: Vec<String> = Vec::new();
        let flush = |d: u64, r: usize, parts: &mut Vec<String>| {
            parts.push(if r > 1 {
                format!("{}x{}", d, r)
            } else {
                d.to_string()
            });
        };
        for d in it.by_ref() {
            if d == prev {
                run += 1;
            } else {
                flush(prev, run, &mut parts);
                prev = d;
                run = 1;
            }
        }
        flush(prev, run, &mut parts);
        println!("{}", parts.join(" -> "));
    } else {
        println!("(none)");
    }
    println!(
        "context switches between dtids: {}",
        slices.windows(2).filter(|w| w[0].dtid != w[1].dtid).count()
    );
    println!();

    // Anomalies.
    println!("=== anomalies ===");
    let mut longest: Vec<&Slice> = slices.iter().collect();
    longest.sort_by_key(|s| -s.wall_dur_ns);
    println!("longest timeslices by wall time:");
    for s in longest.iter().take(3) {
        println!(
            "  {} dtid {}: {} ms wall / {} ms virt / {} syscalls / {} rcbs",
            label(s),
            s.dtid,
            fmt_ms(s.wall_dur_ns),
            fmt_ms(s.virt_dur_ns),
            s.syscalls,
            s.rcb_delta
        );
    }

    // Virtual-time-stuck: syscalls executed while virtual clock froze.
    let stuck: Vec<&Slice> = slices
        .iter()
        .enumerate()
        .filter(|(_i, s)| s.syscalls >= 1 && s.virt_dur_ns == 0)
        .map(|(_, s)| s)
        .collect();
    let stuck_sys: u64 = stuck.iter().map(|s| s.syscalls).sum();
    println!(
        "\nvirtual-time-STUCK: {} of {} slices advanced virtual time by 0 ns \
         while executing {} syscalls total.",
        stuck.len(),
        slices.len(),
        stuck_sys
    );
    if !stuck.is_empty() {
        let idxs: Vec<String> = stuck.iter().map(|s| label(s)).collect();
        let show = idxs.len().min(20);
        println!("  affected: {}{}", idxs[..show].join(", "),
            if idxs.len() > show { format!(", … (+{})", idxs.len() - show) } else { String::new() });
    }

    // Big virtual jumps.
    let jumps: Vec<&Slice> = slices
        .iter()
        .enumerate()
        .filter(|(_i, s)| s.virt_dur_ns >= big_jump)
        .map(|(_, s)| s)
        .collect();
    if !jumps.is_empty() {
        let jsum: i128 = jumps.iter().map(|s| s.virt_dur_ns).sum();
        println!(
            "\nvirtual-time-JUMP: {} slices account for {} ms of the {} ms total virtual advance \
             (a few slices dominate — likely sleep/timeout/nanosleep):",
            jumps.len(),
            fmt_ms(jsum),
            fmt_ms(total_virt)
        );
        for s in &jumps {
            println!(
                "  {} dtid {}: +{} ms virtual over {} syscalls ({} commits, {} ms wall)",
                label(s),
                s.dtid,
                fmt_ms(s.virt_dur_ns),
                s.syscalls,
                s.commits,
                fmt_ms(s.wall_dur_ns)
            );
        }
    }
}
