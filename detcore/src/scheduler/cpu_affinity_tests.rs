/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The thread tree's CPU affinities and homes under `--virtual-cpus`:
//! inherited at clone, kept by exec, reset when a number is given out again.

use super::*;

/// A new task takes its creator's mask, as `dup_task_struct` copies
/// `cpus_mask`, and the next home, so tasks created one after another take
/// the CPUs in turn. The root starts with no mask and home 0.
#[test]
fn a_new_task_inherits_its_creators_mask_and_takes_the_next_home() {
    let mut tree = ThreadTree::default();
    let root = DetPid::from_raw(3);
    let worker = DetTid::from_raw(5);
    let child = DetPid::from_raw(7);
    tree.add_child(root, root, true);
    tree.cpu_affinities.insert(root, CpuSet::only(2));
    tree.add_child(root, worker, false);
    tree.add_child(worker, child, true);
    assert_eq!(tree.cpu_homes[&root], 0);
    assert_eq!(tree.cpu_homes[&worker], 1);
    assert_eq!(tree.cpu_homes[&child], 2);
    assert_eq!(tree.cpu_affinities.get(&worker), Some(&CpuSet::only(2)));
    assert_eq!(tree.cpu_affinities.get(&child), Some(&CpuSet::only(2)));
}

/// A number given out again belongs to a new task: it takes its new
/// creator's mask and a new home, nothing of the task that had it before.
#[test]
fn a_reused_number_starts_from_its_new_creator() {
    let mut tree = ThreadTree::default();
    let root = DetPid::from_raw(3);
    let worker = DetTid::from_raw(5);
    tree.add_child(root, root, true);
    tree.add_child(root, worker, false);
    tree.cpu_affinities.insert(worker, CpuSet::only(3));
    tree.add_child(root, worker, false);
    assert_eq!(tree.cpu_affinities.get(&worker), None);
    assert_eq!(tree.cpu_homes[&worker], 2);
}

/// A thread that execs survives under its leader's number (Linux's
/// `exchange_tids`) with its own mask and home; one with no mask leaves the
/// leader's number none.
#[test]
fn exec_keeps_the_surviving_threads_cpus_under_the_leaders_number() {
    let odd = CpuSet::from_mask_bytes(&[0b1010]);
    let mut tree = ThreadTree::default();
    let root = DetPid::from_raw(3);
    let worker = DetTid::from_raw(5);
    let other = DetTid::from_raw(7);
    tree.add_child(root, root, true);
    tree.add_child(root, worker, false);
    tree.add_child(root, other, false);
    tree.cpu_affinities.insert(root, CpuSet::only(0));
    tree.cpu_affinities.insert(worker, odd);
    tree.transfer_exec_parent_death(worker, root);
    assert_eq!(tree.cpu_affinities.get(&root), Some(&odd));
    assert_eq!(tree.cpu_homes[&root], 1);
    assert!(!tree.cpu_affinities.contains_key(&worker));
    assert!(!tree.cpu_homes.contains_key(&worker));
    tree.transfer_exec_parent_death(other, root);
    assert_eq!(tree.cpu_affinities.get(&root), None);
    assert_eq!(tree.cpu_homes[&root], 2);
}
