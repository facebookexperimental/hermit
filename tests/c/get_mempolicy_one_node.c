/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Asks get_mempolicy the questions libnuma asks, and prints each answer: the
// return value (or errno), the policy, and the first word of the node mask,
// which starts as all ones so that a mask the kernel leaves alone shows as
// such. Under Hermit, which presents a kernel built without NUMA
// (https://github.com/rrnewton/hermit/issues/5269), every memory-policy
// syscall -- get_mempolicy in all these forms, and set_mempolicy, mbind,
// migrate_pages, move_pages and set_mempolicy_home_node below -- answers
// ENOSYS before its arguments are looked at, exactly as that kernel's
// cond_syscall stubs do. (The file keeps its one-node-era name; the
// one-node emulation it was written for is gone.)
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define MPOL_F_NODE 1
#define MPOL_F_ADDR 2
#define MPOL_F_MEMS_ALLOWED 4
#define MPOL_INTERLEAVE 3
#define MPOL_BIND 2

#ifndef SYS_set_mempolicy_home_node
#define SYS_set_mempolicy_home_node 450
#endif

static void report(const char *label, long rc) {
    printf("%s: %s\n", label, rc < 0 ? strerror(errno) : "rc=0");
}

static void ask(const char *label, int with_mask, unsigned long maxnode, void *addr,
                unsigned long flags) {
    int policy = -1;
    unsigned long mask[2] = {~0UL, ~0UL};
    long rc = syscall(SYS_get_mempolicy, &policy, with_mask ? mask : NULL, maxnode, addr, flags);
    if (rc < 0) {
        printf("%s: %s\n", label, strerror(errno));
    } else {
        printf("%s: rc=%ld policy=%d mask=%#lx\n", label, rc, policy, mask[0]);
    }
}

// get_mempolicy(MPOL_F_MEMS_ALLOWED) into a mask in a page mapped with
// `protection`, or unmapped if `protection` is -1.
static void ask_into_page(const char *label, int protection) {
    long page = sysconf(_SC_PAGESIZE);
    void *mapping = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        perror("mmap");
        return;
    }
    if (protection < 0) {
        munmap(mapping, page);
    } else {
        mprotect(mapping, page, protection);
    }
    int policy = -1;
    long rc = syscall(SYS_get_mempolicy, &policy, mapping, 64UL, NULL, MPOL_F_MEMS_ALLOWED);
    printf("%s: %s\n", label, rc < 0 ? strerror(errno) : "rc=0");
    if (protection >= 0) {
        munmap(mapping, page);
    }
}

int main(void) {
    int local = 0;
    ask("mems_allowed", 1, 64, NULL, MPOL_F_MEMS_ALLOWED);
    ask("default policy", 1, 64, NULL, 0);
    ask("mems_allowed with node", 1, 64, NULL, MPOL_F_MEMS_ALLOWED | MPOL_F_NODE);
    ask("node without addr", 0, 0, NULL, MPOL_F_NODE);
    ask("addr without flag", 0, 0, &local, 0);
    ask("unknown flag", 0, 0, NULL, 8);
    ask("maxnode 0", 1, 0, NULL, MPOL_F_MEMS_ALLOWED);
    ask("maxnode 1", 1, 1, NULL, MPOL_F_MEMS_ALLOWED);
    ask("node of addr", 0, 0, &local, MPOL_F_NODE | MPOL_F_ADDR);
    ask("flag above bit 31", 0, 0, NULL, 1UL << 32);
    ask_into_page("read-only mask", PROT_READ);
    ask_into_page("unmapped mask", -1);
    // libnuma's numa_get_interleave_node: the next interleave node.
    unsigned long node0 = 1;
    report("set interleave", syscall(SYS_set_mempolicy, MPOL_INTERLEAVE, &node0, 64UL));
    ask("interleave node", 0, 0, NULL, MPOL_F_NODE);
    // The rest of the family, each with arguments a NUMA kernel would take.
    long page = sysconf(_SC_PAGESIZE);
    void *pages[1] = {&local};
    int status[1] = {-777};
    unsigned long old_nodes = 1;
    report("mbind", syscall(SYS_mbind, &local, (unsigned long)page, MPOL_BIND, &node0, 64UL, 0UL));
    report(
        "migrate_pages",
        syscall(SYS_migrate_pages, 0, 64UL, &old_nodes, &node0));
    report(
        "move_pages", syscall(SYS_move_pages, 0, 1UL, pages, (void *)0, status, 0UL));
    printf("move_pages status untouched: %d\n", status[0] == -777);
    report(
        "set_mempolicy_home_node",
        syscall(SYS_set_mempolicy_home_node, (unsigned long)&local, (unsigned long)page, 0UL, 0UL));
    return 0;
}
