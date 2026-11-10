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
// an attribute every device on that bus has, also through a `..` after the
// device's `subsystem` link. It prints no device's name, so that its output
// is the same on every host with such devices. It looks for the directories
// of memory tiering and of the NUMA and memory policy tunables, which are
// NUMA's too, and reads the attributes such a kernel keeps with a value of
// its own: the first NVMe controller's `numa_node`, and a CXL DAX device's
// `target_node`. Given a directory, `backlinks DIRECTORY` last looks up the
// same objects through the symbolic links the caller made there, by every
// call that looks a path up (see `aliases`).
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
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/xattr.h>
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

/* Prints the first line of the file at `path`, quoted, or the error. */
static void print_contents(const char *label, const char *path) {
    char text[128];
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("%s: %s\n", label, strerrorname_np(errno));
        return;
    }
    ssize_t length = read(fd, text, sizeof(text) - 1);
    close(fd);
    if (length < 0) {
        printf("%s: read %s\n", label, strerrorname_np(errno));
        return;
    }
    text[length] = '\0';
    text[strcspn(text, "\n")] = '\0';
    printf("%s: \"%s\"\n", label, text);
}

/* Lists the directory at `path` for `name`, then opens `name` relative to
 * it. */
static void listed_and_opened(const char *label, const char *path, const char *name) {
    DIR *directory = opendir(path);
    if (directory == NULL) {
        printf("%s: %s\n", label, strerrorname_np(errno));
        return;
    }
    int matches = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        matches += strcmp(entry->d_name, name) == 0;
    }
    printf("%s listing %s: %d\n", label, name, matches);
    char opened[128];
    snprintf(opened, sizeof(opened), "%s openat %s", label, name);
    int fd = openat(dirfd(directory), name, O_RDONLY | O_CLOEXEC);
    report(opened, fd);
    if (fd >= 0) {
        close(fd);
    }
    closedir(directory);
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
    /* The device's directory reached through a `..` after its `subsystem`
     * link, which names the bus's directory. */
    snprintf(path, sizeof(path), "%s/subsystem/../%s/devices/%s", alias, bus, device);
    snprintf(label, sizeof(label), "%s dotdot", bus);
    listed_and_opened(label, path, "numa_node");
    listed_and_opened(label, path, control);
}

/* The NUMA attribute `name` of the first device in `devices`, which a kernel
 * without NUMA registers with a value of its own, listed and read, and the
 * attribute `absent` beside it, which such a kernel does not register. With
 * `provider`, only a device whose parent is on that bus: a DAX device's
 * region's provider. */
static void node_value(const char *label, const char *devices, const char *name,
                       const char *provider, const char *absent) {
    char device[256], directory[512], path[PATH_MAX + 64], text[160];
    if (!smallest_entry(devices, device, sizeof(device))) {
        printf("%s: no device\n", label);
        return;
    }
    snprintf(directory, sizeof(directory), "%s/%s", devices, device);
    if (provider != NULL) {
        char link[PATH_MAX];
        snprintf(path, sizeof(path), "%s/../subsystem", directory);
        ssize_t length = readlink(path, link, sizeof(link) - 1);
        link[length < 0 ? 0 : length] = '\0';
        const char *last = strrchr(link, '/');
        if (last == NULL || strcmp(last + 1, provider) != 0) {
            printf("%s: no %s device\n", label, provider);
            return;
        }
    }
    printf("%s listing %s: %d\n", label, name, listed(directory, name));
    if (absent != NULL) {
        printf("%s listing %s: %d\n", label, absent, listed(directory, absent));
    }
    snprintf(path, sizeof(path), "%s/%s", directory, name);
    snprintf(text, sizeof(text), "%s %s", label, name);
    print_contents(text, path);
}

/* Looks up, through the symbolic links the caller made in `directory`, the
 * objects looked up by their own paths above (`attribute`, the first PCI
 * device's `numa_node`; `control`, its `vendor`; `nodes`, the node
 * directory; `cpu`, cpu0's directory; `device`, the PCI device's directory;
 * `nvme`, the first NVMe controller's `numa_node`, which need not exist)
 * and files of the caller's own (`own`, an ordinary file; `lookalike` and
 * `nvme-lookalike`, directories each with a `numa_node` and a `subsystem`
 * link to the PCI bus or the NVMe class). Through a link, an object is
 * found, or not, as by its own path, whether the call follows the link or
 * fails once its lookup found the object, by every call that looks a path
 * up (readlink, statfs, the xattr calls, inotify, chdir), and after a `..`
 * that leaves it; the link itself is found by lstat, readlink, llistxattr,
 * IN_DONT_FOLLOW and an O_PATH|O_NOFOLLOW open. */
static void aliases(const char *directory) {
    static const char *const names[] = {
        "attribute",       "control",
        "nodes",           "cpu/node0",
        "cpu/topology/core_id", "device/numa_node",
        "device/vendor",   "attribute/x",
        "own",             "lookalike/numa_node",
    };
    char path[PATH_MAX + 64], label[160], target[PATH_MAX];
    struct stat st;
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
        snprintf(path, sizeof(path), "%s/%s", directory, names[i]);
        snprintf(label, sizeof(label), "alias %s", names[i]);
        open_labeled(label, path);
        snprintf(label, sizeof(label), "stat alias %s", names[i]);
        report(label, stat(path, &st));
        snprintf(label, sizeof(label), "lstat alias %s", names[i]);
        report(label, lstat(path, &st));
        snprintf(label, sizeof(label), "access alias %s", names[i]);
        report(label, access(path, F_OK));
    }

    snprintf(path, sizeof(path), "%s/attribute", directory);
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    report("write alias attribute", fd);
    if (fd >= 0) {
        close(fd);
    }
    report("access W_OK alias attribute", access(path, W_OK));
    report("readlink alias attribute", (int)readlink(path, target, sizeof(target)));
    fd = open(path, O_PATH | O_NOFOLLOW | O_CLOEXEC);
    report("O_PATH|O_NOFOLLOW alias attribute", fd);
    if (fd >= 0) {
        close(fd);
    }
    fd = open(path, O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
    report("O_NOFOLLOW alias attribute", fd);
    if (fd >= 0) {
        close(fd);
    }
    report("nofollow access alias attribute",
           faccessat(AT_FDCWD, path, F_OK, AT_SYMLINK_NOFOLLOW));
    report("nofollow fstatat alias attribute",
           fstatat(AT_FDCWD, path, &st, AT_SYMLINK_NOFOLLOW));

    /* O_CREAT through the link fails as by the attribute's own path. */
    char device[256], direct[PATH_MAX];
    if (smallest_entry("/sys/bus/pci/devices", device, sizeof(device))) {
        snprintf(direct, sizeof(direct), "/sys/bus/pci/devices/%s/numa_node", device);
        int by_path = open(direct, O_WRONLY | O_CREAT | O_CLOEXEC, 0644);
        int by_path_errno = errno;
        if (by_path >= 0) {
            close(by_path);
        }
        int by_link = open(path, O_WRONLY | O_CREAT | O_CLOEXEC, 0644);
        int by_link_errno = errno;
        if (by_link >= 0) {
            close(by_link);
        }
        if (by_path < 0 && by_link < 0 && by_path_errno == by_link_errno) {
            printf("creat alias attribute: as by its path\n");
        } else {
            printf("creat alias attribute: path %s link %s\n",
                   by_path < 0 ? strerrorname_np(by_path_errno) : "ok",
                   by_link < 0 ? strerrorname_np(by_link_errno) : "ok");
        }

        /* Through the guest's own descriptor link to the device's
         * directory. */
        snprintf(direct, sizeof(direct), "/sys/bus/pci/devices/%s", device);
        int held = open(direct, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        if (held >= 0) {
            snprintf(direct, sizeof(direct), "/proc/self/fd/%d/numa_node", held);
            report("fd stat numa_node", stat(direct, &st));
            report("fd access numa_node", access(direct, F_OK));
            open_labeled("fd open numa_node", direct);
            snprintf(direct, sizeof(direct), "/proc/self/fd/%d/vendor", held);
            report("fd stat vendor", stat(direct, &st));
            close(held);
        }
    }

    /* A stat whose result overwrites the path it names. */
    int links = open(directory, O_PATH | O_DIRECTORY | O_CLOEXEC);
    if (links >= 0) {
        const char *const overwritten[] = {"attribute", "control"};
        for (size_t i = 0; i < sizeof(overwritten) / sizeof(overwritten[0]); i++) {
            union {
                struct stat st;
                char path[sizeof(struct stat)];
            } overlap;
            snprintf(overlap.path, sizeof(overlap.path), "%s", overwritten[i]);
            snprintf(label, sizeof(label), "overwritten stat alias %s", overwritten[i]);
            /* The call itself: the C library's prototype refuses the
             * overlap. */
            report(label, (int)syscall(SYS_newfstatat, links, overlap.path, &overlap.st, 0));
        }
        close(links);
    }

    snprintf(path, sizeof(path), "%s/nvme", directory);
    print_contents("alias nvme numa_node", path);
    snprintf(path, sizeof(path), "%s/lookalike/numa_node", directory);
    print_contents("alias lookalike numa_node", path);
    snprintf(path, sizeof(path), "%s/nvme-lookalike/numa_node", directory);
    print_contents("alias nvme-lookalike numa_node", path);
    snprintf(path, sizeof(path), "%s/lookalike/../lookalike", directory);
    listed_and_opened("lookalike dotdot", path, "numa_node");

    /* The other calls that look a path up, each beside a control. */
    struct statfs fs;
    snprintf(path, sizeof(path), "%s/cpu/node0", directory);
    report("readlink alias cpu/node0", (int)readlink(path, target, sizeof(target)));
    snprintf(path, sizeof(path), "%s/cpu/subsystem", directory);
    report("readlink alias cpu/subsystem", (int)readlink(path, target, sizeof(target)));
    snprintf(path, sizeof(path), "%s/attribute", directory);
    report("statfs alias attribute", statfs(path, &fs));
    report("listxattr alias attribute", (int)listxattr(path, NULL, 0));
    report("llistxattr alias attribute", (int)llistxattr(path, NULL, 0));
    int notify = inotify_init1(IN_CLOEXEC);
    report("inotify alias attribute", inotify_add_watch(notify, path, IN_ATTRIB));
    report("inotify IN_DONT_FOLLOW alias attribute",
           inotify_add_watch(notify, path, IN_ATTRIB | IN_DONT_FOLLOW));
    snprintf(path, sizeof(path), "%s/control", directory);
    report("statfs alias control", statfs(path, &fs));
    report("listxattr alias control", (int)listxattr(path, NULL, 0));
    report("inotify alias control", inotify_add_watch(notify, path, IN_ATTRIB));
    if (notify >= 0) {
        close(notify);
    }
    snprintf(path, sizeof(path), "%s/nodes/..", directory);
    report("stat alias nodes/..", stat(path, &st));
    snprintf(path, sizeof(path), "%s/cpu/..", directory);
    report("stat alias cpu/..", stat(path, &st));
    char here[PATH_MAX];
    if (getcwd(here, sizeof(here)) != NULL) {
        const char *const directories[] = {"nodes", "device"};
        for (size_t i = 0; i < sizeof(directories) / sizeof(directories[0]); i++) {
            snprintf(path, sizeof(path), "%s/%s", directory, directories[i]);
            snprintf(label, sizeof(label), "chdir alias %s", directories[i]);
            int changed = chdir(path);
            report(label, changed);
            if (changed == 0 && chdir(here) != 0) {
                printf("chdir back: %s\n", strerrorname_np(errno));
            }
        }
    }
}

static int backlinks(const char *directory) {
    int failed = bound_entry("/sys/devices/system/cpu/cpu0", "node0");
    failed |= bound_entry("/sys/bus/cpu/devices/cpu0", "node0");
    probe_backlink("/sys/devices/system/cpu/cpu0/node0");
    probe_backlink("/sys/bus/cpu/devices/cpu0/node0");
    probe_backlink("/sys/bus/cpu/drivers/processor/cpu0/node0");
    open_and_close("/sys/devices/system/cpu/cpu0/node0/cpulist");
    open_and_close("/sys/devices/system/cpu/cpu0/topology/core_id");
    device_attributes("pci", "vendor");
    device_attributes("platform", "modalias");
    /* The directories of memory tiering and of the NUMA and memory policy
     * tunables, which such a kernel does not have either. */
    const char *directories[][2] = {
        {"/sys/bus", "memory_tiering"},
        {"/sys/devices/virtual", "memory_tiering"},
        {"/sys/kernel/mm", "numa"},
        {"/sys/kernel/mm", "mempolicy"},
    };
    for (size_t i = 0; i < sizeof(directories) / sizeof(directories[0]); i++) {
        char path[256], label[300];
        printf("%s listing %s: %d\n", directories[i][0], directories[i][1],
               listed(directories[i][0], directories[i][1]));
        snprintf(path, sizeof(path), "%s/%s", directories[i][0], directories[i][1]);
        snprintf(label, sizeof(label), "access %s", path);
        report(label, access(path, F_OK));
    }
    open_and_close("/sys/kernel/mm/numa/demotion_enabled");
    node_value("nvme", "/sys/class/nvme", "numa_node", NULL, NULL);
    node_value("dax", "/sys/bus/dax/devices", "target_node", "cxl", "numa_node");
    if (directory != NULL) {
        aliases(directory);
    }
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
    if ((argc == 2 || argc == 3) && strcmp(argv[1], "backlinks") == 0) {
        return backlinks(argc == 3 ? argv[2] : NULL);
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
