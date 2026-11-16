//! Export Detcore's per-syscall policy as TSV by linking this checkout's own
//! `detcore` crate. Nothing here re-parses source: every column comes from a
//! public Detcore function.
//!
//! This file is a template, not a standalone script. `syscall-pages.rs
//! facts` / `check` generates a small cargo project under
//! `ignored/syscall-pages/facts-build/<sha>/` from it (this file becomes its
//! `src/main.rs` with the hermit SHA substituted in), builds it against the
//! checkout's `detcore` and the pinned `reverie/` tree, and runs it with
//! `--expect-sha <sha>`. The compiled-in SHA makes a stale build refuse
//! instead of reporting another revision's facts, and the caller refuses to
//! build at all from a checkout whose detcore, detcore-model, reverie tree,
//! or Cargo.lock is dirty, so the facts always describe exactly HEAD.
//!
//! Output columns: name, nr, class, refused, strict_only_refused,
//! subscribed in run mode, subscribed with `passthru_opt` (record/replay).

use std::collections::BTreeSet;
use std::process::exit;

use detcore::Config;
use detcore::Detcore;
use reverie::Tool;

/// Substituted by `syscall-pages.rs` when it generates the build.
const SOURCE_SHA: &str = "@HERMIT_SHA@";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let expect = match args.as_slice() {
        [flag, sha] if flag == "--expect-sha" => sha.clone(),
        _ => {
            eprintln!("usage: syscall-pages-facts --expect-sha <40-hex hermit sha>");
            exit(2);
        }
    };
    if SOURCE_SHA != expect {
        eprintln!(
            "syscall-pages-facts: STALE BUILD: compiled against hermit {SOURCE_SHA}, asked for {expect}"
        );
        exit(3);
    }
    let run_cfg = Config {
        sequentialize_threads: true,
        deterministic_io: true,
        ..Default::default()
    };
    let rr_cfg = Config {
        passthru_opt: true,
        ..run_cfg.clone()
    };
    let run: BTreeSet<_> = <Detcore as Tool>::subscriptions(&run_cfg)
        .iter_syscalls()
        .collect();
    let rr: BTreeSet<_> = <Detcore as Tool>::subscriptions(&rr_cfg)
        .iter_syscalls()
        .collect();
    println!("# hermit {SOURCE_SHA}");
    println!("name\tnr\tclass\trefused\tstrict_only_refused\tsub_run\tsub_record_replay");
    for s in detcore::all_pinned_syscalls() {
        let class = if detcore::is_unsupported_syscall(s) {
            "unsupported"
        } else if detcore::is_determinized_syscall(s) {
            "determinized"
        } else {
            "passthrough"
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            s.name(),
            s.id(),
            class,
            detcore::is_deterministically_refused_syscall(s) as u8,
            detcore::is_strict_only_deterministic_refusal_syscall(s) as u8,
            run.contains(&s) as u8,
            rr.contains(&s) as u8
        );
    }
}
