/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Probe the sched_setattr argument contract and print one line per case.
 *
 * This fixture asserts nothing itself. Its driver
 * (hermit-cli/tests/sched_setattr_abi.rs) runs it natively and under Hermit and
 * compares the two transcripts byte for byte, so the expectation is "Hermit
 * agrees with this kernel" rather than a table baked in here that could be
 * wrong in both places at once.
 *
 * Each case runs in a forked child, so a request that succeeds cannot change
 * the scheduling of the process doing the probing.
 *
 * Deliberately excluded: every case whose answer depends on the host's
 * configuration or the caller's privileges rather than on the interface, since
 * such a case would make this test's verdict depend on the machine. That means
 * no real-time priority or negative nice (EPERM without the capability or
 * rlimit), no util-clamp on a VER1 buffer (EOPNOTSUPP without
 * CONFIG_UCLAMP_TASK), and no SCHED_DEADLINE period near the
 * sysctl_sched_dl_period_{min,max} bound.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define SCHED_ATTR_SIZE_VER0 48u
/* The kernel's own sizeof(struct sched_attr): what err_size stores back. */
#define SCHED_ATTR_KERNEL_SIZE 56u

#define SCHED_FLAG_RESET_ON_FORK 0x01u
#define SCHED_FLAG_RECLAIM 0x02u
#define SCHED_FLAG_DL_OVERRUN 0x04u
#define SCHED_FLAG_KEEP_POLICY 0x08u
#define SCHED_FLAG_KEEP_PARAMS 0x10u
#define SCHED_FLAG_UTIL_CLAMP_MIN 0x20u

/* Field offsets in the UAPI struct sched_attr. */
#define OFF_SIZE 0
#define OFF_POLICY 4
#define OFF_FLAGS 8
#define OFF_PRIORITY 20
#define OFF_RUNTIME 24
#define OFF_DEADLINE 32
#define OFF_PERIOD 40

/* Page-and-a-bit, so an oversized declared size has real memory behind it. */
static unsigned char buf[8192] __attribute__((aligned(4096)));

struct req {
    pid_t pid;
    int null_attr;
    void *bad_attr; /* non-NULL: use this pointer instead of buf */
    unsigned int flags;
    uint32_t size;
    uint32_t policy;
    uint64_t sched_flags;
    uint32_t priority;
    uint64_t runtime;
    uint64_t deadline;
    uint64_t period;
    uint32_t tail_off; /* 0 = leave the tail zeroed */
};

static void put32(unsigned char *p, size_t off, uint32_t v) {
    memcpy(p + off, &v, sizeof(v));
}

static void put64(unsigned char *p, size_t off, uint64_t v) {
    memcpy(p + off, &v, sizeof(v));
}

static uint32_t get32(const unsigned char *p, size_t off) {
    uint32_t v;
    memcpy(&v, p + off, sizeof(v));
    return v;
}

/* Run one case in a child and print its return, errno and post-call size. */
static void probe(const char *name, struct req r) {
    struct {
        long ret;
        int err;
        uint32_t size_after;
    } out = {0, 0, 0};

    int fds[2];
    if (pipe(fds) != 0) {
        printf("%-44s PIPE-FAILED\n", name);
        return;
    }

    pid_t child = fork();
    if (child == 0) {
        close(fds[0]);
        memset(buf, 0, sizeof(buf));
        put32(buf, OFF_SIZE, r.size);
        put32(buf, OFF_POLICY, r.policy);
        put64(buf, OFF_FLAGS, r.sched_flags);
        put32(buf, OFF_PRIORITY, r.priority);
        put64(buf, OFF_RUNTIME, r.runtime);
        put64(buf, OFF_DEADLINE, r.deadline);
        put64(buf, OFF_PERIOD, r.period);
        if (r.tail_off) buf[r.tail_off] = 0xff;

        errno = 0;
        void *attr_ptr = r.null_attr ? NULL : (r.bad_attr ? r.bad_attr : buf);
        long ret = syscall(SYS_sched_setattr, r.pid, attr_ptr, r.flags);
        out.ret = ret;
        out.err = ret == 0 ? 0 : errno;
        out.size_after = get32(buf, OFF_SIZE);
        ssize_t w = write(fds[1], &out, sizeof(out));
        (void)w;
        _exit(0);
    }

    close(fds[1]);
    ssize_t n = read(fds[0], &out, sizeof(out));
    (void)n;
    close(fds[0]);
    int status;
    waitpid(child, &status, 0);

    printf("%-44s ret=%ld errno=%d size_after=%u\n", name, out.ret, out.err,
           out.size_after);
}

int main(void) {
    struct req base = {0};
    base.size = SCHED_ATTR_SIZE_VER0;

    /* The first clause: !uattr || pid < 0 || flags -- all EINVAL. */
    struct req r = base;
    r.null_attr = 1;
    probe("null attr", r);
    r = base;
    r.pid = -1;
    probe("negative pid", r);
    r = base;
    r.flags = 1;
    probe("nonzero flags", r);

    /* sched_copy_attr size rules, including the zero-size ABI quirk. */
    r = base;
    r.size = 0;
    probe("size=0 is the VER0 quirk", r);
    for (uint32_t size = 1; size <= 4097; size = size == 1      ? 47
                                              : size == 47      ? 48
                                              : size == 48      ? 56
                                              : size == 56      ? 57
                                              : size == 57      ? 4096
                                              : size == 4096    ? 4097
                                                                : 4098) {
        char name[64];
        snprintf(name, sizeof(name), "size=%u", size);
        r = base;
        r.size = size;
        probe(name, r);
        if (size == 4097) break;
    }

    /* Trailing bytes past the kernel's struct must be zero. */
    r = base;
    r.size = SCHED_ATTR_KERNEL_SIZE + 8;
    probe("oversized, zero tail", r);
    r = base;
    r.size = SCHED_ATTR_KERNEL_SIZE + 8;
    r.tail_off = SCHED_ATTR_KERNEL_SIZE;
    probe("oversized, nonzero tail", r);
    r = base;
    r.size = 4096;
    r.tail_off = 4095;
    probe("page-sized, nonzero tail at 4095", r);

    /* Policy validity.
     *
     * SCHED_EXT (7) IS DELIBERATELY SKIPPED, and skipping it is the point
     * rather than an omission. Whether the kernel accepts policy 7 depends on
     * whether it was built with CONFIG_SCHED_CLASS_EXT, so the NATIVE answer
     * varies from machine to machine. This probe exists to be compared
     * native-against-sandboxed, and a bracket whose expected value depends on
     * the host is not a determinism test -- it would report a Hermit defect on
     * one box and pass on the next, for a reason that has nothing to do with
     * Hermit. The sandbox's own treatment of policy 7 is pinned by the unit
     * test `sched_ext_is_a_valid_policy_and_sched_iso_is_not`, which is
     * host-independent because it never asks the host.
     *
     * 8 is unassigned on every kernel, so it stays: it is the neighbour that
     * proves the loop can still see a rejection. */
    for (uint32_t policy = 0; policy <= 8; policy++) {
        char name[64];
        if (policy == 7) {
            continue;
        }
        snprintf(name, sizeof(name), "policy=%u", policy);
        r = base;
        r.policy = policy;
        /* The real-time policies need a nonzero priority to be well formed,
         * but a nonzero priority needs privilege, so probe them at priority 0
         * where the answer is a pure-ABI EINVAL. */
        probe(name, r);
    }
    r = base;
    r.policy = 99;
    probe("policy=99", r);
    r = base;
    r.policy = 0x80000000u;
    probe("policy sign bit set", r);
    r = base;
    r.policy = 0x80000000u;
    r.sched_flags = SCHED_FLAG_KEEP_POLICY;
    probe("policy sign bit set + KEEP_POLICY", r);
    r = base;
    r.policy = 99;
    r.sched_flags = SCHED_FLAG_KEEP_POLICY;
    probe("policy=99 + KEEP_POLICY", r);
    /* KEEP_POLICY does not switch the policy-dependent rules off; it makes them
     * apply to the CURRENT policy. Started from a plain SCHED_OTHER thread, a
     * nonzero priority is refused, and that refusal is the only thing that can
     * tell "reuse the current policy" apart from "skip the checks". */
    r = base;
    r.sched_flags = SCHED_FLAG_KEEP_POLICY;
    r.priority = 1;
    probe("KEEP_POLICY with priority=1", r);

    /* sched_flags validity. */
    r = base;
    r.sched_flags = 0x80;
    probe("undefined sched_flags bit", r);
    r = base;
    r.sched_flags = SCHED_FLAG_RESET_ON_FORK;
    probe("RESET_ON_FORK", r);
    r = base;
    r.sched_flags = SCHED_FLAG_RECLAIM;
    probe("RECLAIM", r);
    r = base;
    r.sched_flags = SCHED_FLAG_DL_OVERRUN;
    probe("DL_OVERRUN", r);
    r = base;
    r.sched_flags = SCHED_FLAG_KEEP_PARAMS;
    probe("KEEP_PARAMS", r);
    r = base;
    r.sched_flags = SCHED_FLAG_UTIL_CLAMP_MIN;
    probe("UTIL_CLAMP_MIN on a VER0 buffer", r);

    /* Priority must agree with the policy: rt_policy(p) != (prio != 0). */
    r = base;
    r.priority = 1;
    probe("SCHED_OTHER with priority=1", r);
    r = base;
    r.policy = 3;
    r.priority = 1;
    probe("SCHED_BATCH with priority=1", r);
    r = base;
    r.policy = 5;
    r.priority = 1;
    probe("SCHED_IDLE with priority=1", r);
    r = base;
    r.policy = 1;
    probe("SCHED_FIFO with priority=0", r);
    r = base;
    r.policy = 2;
    probe("SCHED_RR with priority=0", r);
    r = base;
    r.policy = 1;
    r.priority = 100;
    probe("SCHED_FIFO with priority=100", r);

    /* SCHED_DEADLINE parameter checking, at the pure-ABI edges only. */
    r = base;
    r.policy = 6;
    probe("SCHED_DEADLINE all-zero params", r);
    r = base;
    r.policy = 6;
    r.runtime = 90000000ULL;
    r.deadline = 30000000ULL;
    r.period = 30000000ULL;
    probe("SCHED_DEADLINE runtime>deadline", r);
    r = base;
    r.policy = 6;
    r.runtime = 512;
    r.deadline = 30000000ULL;
    r.period = 30000000ULL;
    probe("SCHED_DEADLINE runtime below DL_SCALE", r);

    /* Ordering: the pid lookup sits between the two groups of checks, so a
     * nonexistent pid reports ESRCH for the far-side rules and the rule's own
     * errno for the near-side ones. */
    const pid_t absent = 0x3fffffff;
    r = base;
    r.pid = absent;
    probe("ORDER absent pid, well formed", r);
    r = base;
    r.pid = absent;
    r.sched_flags = 0x80;
    probe("ORDER absent pid vs bad sched_flags", r);
    r = base;
    r.pid = absent;
    r.policy = 99;
    probe("ORDER absent pid vs bad policy", r);
    r = base;
    r.pid = absent;
    r.priority = 1;
    probe("ORDER absent pid vs bad priority", r);
    r = base;
    r.pid = absent;
    r.policy = 0x80000000u;
    probe("ORDER absent pid vs negative policy", r);
    r = base;
    r.pid = absent;
    r.sched_flags = SCHED_FLAG_UTIL_CLAMP_MIN;
    probe("ORDER absent pid vs util-clamp size", r);
    r = base;
    r.pid = absent;
    r.size = 1;
    probe("ORDER absent pid vs bad size", r);

    /* A non-null but unmapped pointer faults: EFAULT, not EINVAL and not the
     * backend's own memory-access errno. */
    r = base;
    r.bad_attr = (void *)0x10;
    probe("unmapped attr pointer", r);

    /* And a well-formed request is still accepted. */
    r = base;
    probe("well-formed SCHED_OTHER", r);

    return 0;
}
