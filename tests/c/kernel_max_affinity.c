/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Sizes a CPU set from /sys/devices/system/cpu/kernel_max + 1, as the kernel
// documents, and then from sysconf(_SC_NPROCESSORS_CONF), and asks
// sched_getaffinity for each. Prints the size used, the result and the number
// of CPUs in the set.
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

// Asks sched_getaffinity for a set of `cpus` CPUs; prints `label`, the count
// the set was sized from, its size, the result and the CPUs in it.
static int ask(const char *label, long cpus) {
    size_t size = CPU_ALLOC_SIZE(cpus);
    cpu_set_t *set = CPU_ALLOC(cpus);
    if (set == NULL) {
        printf("CPU_ALLOC(%ld) failed\n", cpus);
        return 2;
    }
    int rc = sched_getaffinity(0, size, set);
    int saved = errno;
    printf("%s=%ld setsize=%zu sched_getaffinity rc=%d %s count=%d\n", label, cpus, size, rc,
           rc == 0 ? "ok" : strerror(saved), rc == 0 ? CPU_COUNT_S(size, set) : -1);
    CPU_FREE(set);
    return rc == 0 ? 0 : 1;
}

int main(void) {
    FILE *file = fopen("/sys/devices/system/cpu/kernel_max", "r");
    int kernel_max = -1;
    if (file == NULL || fscanf(file, "%d", &kernel_max) != 1) {
        printf("kernel_max unreadable\n");
        return 2;
    }
    fclose(file);
    // kernel_max is the highest CPU number, so the set holds kernel_max + 1.
    printf("kernel_max=%d ", kernel_max);
    int failed = ask("cpus", kernel_max + 1);
    failed |= ask("nprocessors_conf", sysconf(_SC_NPROCESSORS_CONF));
    return failed;
}
