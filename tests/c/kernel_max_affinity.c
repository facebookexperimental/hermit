/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Sizes a CPU set from /sys/devices/system/cpu/kernel_max + 1, as the kernel
// documents, and asks sched_getaffinity for it. Prints the size used, the
// result and the number of CPUs in the set.
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    FILE *file = fopen("/sys/devices/system/cpu/kernel_max", "r");
    int kernel_max = -1;
    if (file == NULL || fscanf(file, "%d", &kernel_max) != 1) {
        printf("kernel_max unreadable\n");
        return 2;
    }
    fclose(file);
    int cpus = kernel_max + 1;
    size_t size = CPU_ALLOC_SIZE(cpus);
    cpu_set_t *set = CPU_ALLOC(cpus);
    if (set == NULL) {
        printf("CPU_ALLOC(%d) failed\n", cpus);
        return 2;
    }
    int rc = sched_getaffinity(0, size, set);
    int saved = errno;
    printf("kernel_max=%d setsize=%zu sched_getaffinity rc=%d %s count=%d\n", kernel_max, size,
           rc, rc == 0 ? "ok" : strerror(saved), rc == 0 ? CPU_COUNT_S(size, set) : -1);
    CPU_FREE(set);
    return rc == 0 ? 0 : 1;
}
