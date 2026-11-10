/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Looks up the CPUs other than number 0 and every NUMA node that the
// sysfs listings leave out, and registers an rseq area, and prints what
// each call returns. Under Hermit, which presents one CPU and a kernel
// without NUMA, every such lookup fails with ENOENT, and rseq fails
// with ENOSYS under the default policy; natively, on a host with more
// than one CPU, the cpu1 lookups succeed. Its `backlinks` mode lists a
// present CPU's directory and looks up its `node<N>` backlink by every
// spelling, which a kernel without NUMA does not create. It then does the
// same for the `numa_node` attribute of the first PCI device and the first
// platform device, which such a kernel does not register either, next to
// an attribute every device on that bus has. It prints no device's name, so
// that its output is the same on every host with such devices.
//
// The lookups name the absent entry absolutely, below it, relative to a
// directory descriptor, through /sys/bus (including the processor driver's
// link), with stat, access and readlink, and the node0 and cpu0 names stay
// found. rseq is registered with an aligned
// 32-byte area and the signature glibc uses. If glibc already registered its
// own area at startup, Linux refuses this different area with EINVAL.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static void report(const char *what, int rc) {
    if (rc < 0) {
        printf("%s: %s\n", what, strerrorname_np(errno));
    } else {
        printf("%s: ok\n", what);
    }
}

static void open_and_close(const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    report(path, fd);
    if (fd >= 0) {
        close(fd);
    }
}

static int bound_entry(const char *path, const char *name);

static void probe_backlink(const char *path) {
    char label[512];
    struct stat st;
    open_and_close(path);

    snprintf(label, sizeof(label), "stat %s", path);
    report(label, stat(path, &st));
    snprintf(label, sizeof(label), "lstat %s", path);
    report(label, lstat(path, &st));
    char target[256];
    snprintf(label, sizeof(label), "readlink %s", path);
    report(label, (int)readlink(path, target, sizeof(target)));
}

/* How many entries of the directory at `path` are named `name`. */
static int listed(const char *path, const char *name) {
    DIR *directory = opendir(path);
    if (directory == NULL) {
        return -1;
    }
    int matches = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        matches += strcmp(entry->d_name, name) == 0;
    }
    closedir(directory);
    return matches;
}

/* The smallest name in the directory at `path` other than `.` and `..`, so
 * that every listing order picks the same one. */
static int smallest_entry(const char *path, char *name, size_t size) {
    DIR *directory = opendir(path);
    if (directory == NULL) {
        return 0;
    }
    int found = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) {
            continue;
        }
        if (!found || strcmp(entry->d_name, name) < 0) {
            snprintf(name, size, "%s", entry->d_name);
            found = 1;
        }
    }
    closedir(directory);
    return found;
}

static void open_labeled(const char *label, const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    report(label, fd);
    if (fd >= 0) {
        close(fd);
    }
}

/* The `numa_node` attribute of the first device on `bus`, and the `control`
 * attribute every device there has: listed, opened, stat'ed and accessed
 * through the bus's alias for the device, opened through the device's own
 * path, through its `subsystem` link, relative to its directory, and below
 * it. */
static void device_attributes(const char *bus, const char *control) {
    char devices[64], device[256], alias[512], path[PATH_MAX + 64], label[128];
    char canonical[PATH_MAX];
    struct stat st;
    snprintf(devices, sizeof(devices), "/sys/bus/%s/devices", bus);
    if (!smallest_entry(devices, device, sizeof(device))) {
        printf("%s: no device\n", bus);
        return;
    }
    snprintf(alias, sizeof(alias), "%s/%s", devices, device);
    printf("%s listing numa_node: %d\n", bus, listed(alias, "numa_node"));
    printf("%s listing %s: %d\n", bus, control, listed(alias, control));
    const char *names[] = {"numa_node", control};
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
        snprintf(path, sizeof(path), "%s/%s", alias, names[i]);
        snprintf(label, sizeof(label), "%s %s", bus, names[i]);
        open_labeled(label, path);
        snprintf(label, sizeof(label), "stat %s %s", bus, names[i]);
        report(label, stat(path, &st));
        snprintf(label, sizeof(label), "access %s %s", bus, names[i]);
        report(label, access(path, F_OK));
    }
    if (realpath(alias, canonical) != NULL) {
        snprintf(path, sizeof(path), "%s/numa_node", canonical);
        snprintf(label, sizeof(label), "%s device path numa_node", bus);
        open_labeled(label, path);
    }
    snprintf(path, sizeof(path), "%s/subsystem/devices/%s/numa_node", alias, device);
    snprintf(label, sizeof(label), "%s subsystem numa_node", bus);
    open_labeled(label, path);
    snprintf(path, sizeof(path), "%s/numa_node/x", alias);
    snprintf(label, sizeof(label), "%s below numa_node", bus);
    open_labeled(label, path);
    int directory = open(alias, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
        int fd = openat(directory, names[i], O_RDONLY | O_CLOEXEC);
        snprintf(label, sizeof(label), "%s openat %s", bus, names[i]);
        report(label, fd);
        if (fd >= 0) {
            close(fd);
        }
    }
    if (directory >= 0) {
        close(directory);
    }
}

static int backlinks(void) {
    int failed = bound_entry("/sys/devices/system/cpu/cpu0", "node0");
    failed |= bound_entry("/sys/bus/cpu/devices/cpu0", "node0");
    probe_backlink("/sys/devices/system/cpu/cpu0/node0");
    probe_backlink("/sys/bus/cpu/devices/cpu0/node0");
    probe_backlink("/sys/bus/cpu/drivers/processor/cpu0/node0");
    open_and_close("/sys/devices/system/cpu/cpu0/node0/cpulist");
    open_and_close("/sys/devices/system/cpu/cpu0/topology/core_id");
    device_attributes("pci", "vendor");
    device_attributes("platform", "modalias");
    return failed;
}

struct rseq_area {
    uint32_t cpu_id_start;
    uint32_t cpu_id;
    uint64_t rseq_cs;
    uint32_t flags;
    uint32_t node_id;
    uint32_t mm_cid;
    char end[];
} __attribute__((aligned(32)));

static int bound_entry(const char *path, const char *name) {
    DIR *directory = opendir(path);
    if (directory == NULL) {
        perror("opendir bound directory");
        return 1;
    }
    int matches = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        matches += strcmp(entry->d_name, name) == 0;
    }
    printf("bound listing %s: %d\n", name, matches);
    int fd = openat(dirfd(directory), name, O_RDONLY | O_CLOEXEC);
    report("bound openat", fd);
    if (fd >= 0) {
        close(fd);
    }
    closedir(directory);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "backlinks") == 0) {
        return backlinks();
    }
    if (argc == 4 && strcmp(argv[1], "bound") == 0) {
        return bound_entry(argv[2], argv[3]);
    }
    if (argc != 1) {
        return 2;
    }
    open_and_close("/sys/devices/system/node/node1/cpulist");
    open_and_close("/sys/devices/system/cpu/cpu1/topology/core_id");
    open_and_close("/sys/devices/system/cpu/cpu0/topology/core_id");
    open_and_close("/sys/devices/system/node/node0/cpulist");

    struct stat st;
    report("stat /sys/devices/system/cpu/cpu1",
           stat("/sys/devices/system/cpu/cpu1", &st));
    report("access /sys/devices/system/node/node1",
           access("/sys/devices/system/node/node1", F_OK));
    char target[256];
    report("readlink /sys/bus/cpu/devices/cpu1",
           (int)readlink("/sys/bus/cpu/devices/cpu1", target, sizeof(target)));
    report("readlink /sys/bus/cpu/devices/cpu0",
           (int)readlink("/sys/bus/cpu/devices/cpu0", target, sizeof(target)));
    report("readlink /sys/bus/cpu/drivers/processor/cpu1",
           (int)readlink("/sys/bus/cpu/drivers/processor/cpu1", target, sizeof(target)));

    int directory = open("/sys/devices/system/cpu", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (directory < 0) {
        perror("open /sys/devices/system/cpu");
        return 1;
    }
    int online = openat(directory, "cpu1/online", O_RDONLY | O_CLOEXEC);
    report("openat cpu1/online", online);
    if (online >= 0) {
        close(online);
    }
    report("fstatat cpu1", fstatat(directory, "cpu1", &st, 0));
    close(directory);

    static struct rseq_area area;
    memset(&area, 0, sizeof(area));
    area.cpu_id = (uint32_t)-1;
    report("rseq", (int)syscall(SYS_rseq, &area, sizeof(area), 0, 0x53053053));
    printf("rseq cpu_id: %d\n", (int)area.cpu_id);
    return 0;
}
