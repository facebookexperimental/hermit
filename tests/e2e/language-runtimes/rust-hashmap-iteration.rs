// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// End-to-end Rust standard-library determinism fixture.
//
// Rust's default HashMap/HashSet hasher (RandomState) seeds SipHash from OS
// entropy (getrandom), so the iteration order of hash containers varies every
// run natively. Under Hermit --strict that entropy is determinized, so the
// iteration order -- and the file-I/O roundtrip and std::time timestamp derived
// alongside it -- become bitwise reproducible. A BTreeMap provides an ordered
// cross-check whose output is stable regardless of hashing.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

// 64-bit FNV-1a fold: a pure function that compresses order-sensitive material
// into a compact, well-defined fingerprint.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

fn main() {
    let words = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta",
        "iota", "kappa", "lambda", "mu", "nu", "xi", "omicron", "pi",
    ];

    // HashSet iteration order is RandomState-seed sensitive.
    let mut hset: HashSet<&str> = HashSet::new();
    for w in &words {
        hset.insert(w);
    }
    let set_order = hset.into_iter().collect::<Vec<_>>().join(",");

    // HashMap iteration order is likewise seed sensitive.
    let mut hmap: HashMap<&str, usize> = HashMap::new();
    for (i, w) in words.iter().enumerate() {
        hmap.insert(w, i * i);
    }
    let map_order = hmap
        .iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect::<Vec<_>>()
        .join(",");

    // BTreeMap is canonically ordered: a stable cross-check independent of
    // hashing, so its output is identical natively and under Hermit.
    let btree: BTreeMap<&str, usize> =
        words.iter().enumerate().map(|(i, w)| (*w, i)).collect();
    let btree_order = btree
        .iter()
        .map(|(k, v)| format!("{}:{}", k, v))
        .collect::<Vec<_>>()
        .join(",");

    // File I/O roundtrip through E2E_TMPDIR (created first; Hermit gives the
    // guest a fresh isolated /tmp per repeat).
    let dir = env::var("E2E_TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let mut path = PathBuf::from(&dir);
    fs::create_dir_all(&path).expect("create E2E_TMPDIR");
    path.push("hermit-rust-hashmap.txt");
    let payload = format!("{}\n{}\n{}\n", set_order, map_order, btree_order);
    fs::write(&path, payload.as_bytes()).expect("write payload");
    let readback = fs::read(&path).expect("read payload");

    // std::time: determinized to Hermit's virtual epoch under --strict.
    let epoch_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs();

    println!(
        "RUSTHASH set_order={} map_fnv={:016x} btree={} epoch_s={} bytes={} \
         payload_fnv={:016x} roundtrip={}",
        set_order,
        fnv1a(&map_order),
        btree_order,
        epoch_s,
        readback.len(),
        fnv1a(&payload),
        u8::from(readback == payload.as_bytes()),
    );
}
