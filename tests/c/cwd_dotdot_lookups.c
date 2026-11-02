/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Looks up names relative to working directories whose path getcwd cannot
// give, which Linux still walks `..` out of, and prints, for each, the errno
// of a stat and of an open (O_DIRECTORY for a directory), 0 for success, or
// of the open alone: a removed directory, /tmp/dead, entered and then
// removed (getcwd: ENOENT); and one whose path is longer than PATH_MAX,
// entered while short and then moved below a long path (getcwd:
// ENAMETOOLONG, which glibc answers as ERANGE). Their leading `..` reach
// /proc/fs, a name Hermit hides at every --proc level, /proc itself,
// /proc/uptime spelled with a trailing `/` (a file, so no directory), and the
// ordinary /tmp/sibling; below the long directory are its own proc/fs and
// x, through which `x/../proc/fs` is that proc/fs again. From each, the same
// climb to /proc/uptime padded with separators to 4095 bytes resolves, and
// to 4096 and 4097 bytes is refused with ENAMETOOLONG, as Linux refuses any
// name of PATH_MAX bytes or more before it walks. Controls: /proc/fs by its
// absolute spelling, and ../sibling from /tmp/live, whose path getcwd gives.
// Last it prints the descriptor the next open gets, which no lookup before
// it may have moved.
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static const char *errno_name(int error) {
    switch (error) {
    case 0:
        return "0";
    case ENOENT:
        return "ENOENT";
    case EACCES:
        return "EACCES";
    case ENOTDIR:
        return "ENOTDIR";
    case ELOOP:
        return "ELOOP";
    case EPERM:
        return "EPERM";
    case ERANGE:
        return "ERANGE";
    case ENAMETOOLONG:
        return "ENAMETOOLONG";
    default:
        return strerror(error);
    }
}

static void look_up(const char *where, const char *shown, const char *name, int directory) {
    struct stat status;
    errno = 0;
    int stat_error = stat(name, &status) == 0 ? 0 : errno;
    errno = 0;
    int fd = open(name, O_RDONLY | O_CLOEXEC | (directory ? O_DIRECTORY : 0));
    int open_error = fd >= 0 ? 0 : errno;
    if (fd >= 0) {
        close(fd);
    }
    printf("%s %s: stat=%s open=%s\n", where, shown, errno_name(stat_error),
           errno_name(open_error));
}

static void open_only(const char *where, const char *shown, const char *name, int directory) {
    errno = 0;
    int fd = open(name, O_RDONLY | O_CLOEXEC | (directory ? O_DIRECTORY : 0));
    int open_error = fd >= 0 ? 0 : errno;
    if (fd >= 0) {
        close(fd);
    }
    printf("%s %s: open=%s\n", where, shown, errno_name(open_error));
}

static void print_getcwd(const char *where) {
    char path[PATH_MAX];
    errno = 0;
    const char *got = getcwd(path, sizeof(path));
    printf("%s getcwd: %s\n", where, got != NULL ? "a path" : errno_name(errno));
}

// `count` `..` components, then `rest`, into `buffer`.
static const char *climbing(char *buffer, size_t size, int count, const char *rest) {
    size_t used = 0;
    buffer[0] = '\0';
    for (int i = 0; i < count && used + 3 < size; i++) {
        memcpy(buffer + used, "../", 4);
        used += 3;
    }
    snprintf(buffer + used, size - used, "%s", rest);
    return buffer;
}

// `count` `..` components, then as many separators as make the name `total`
// bytes long with `rest`, then `rest`, into `buffer` (2 * PATH_MAX bytes).
static const char *padded(char *buffer, int count, size_t total, const char *rest) {
    size_t used = 0;
    for (int i = 0; i < count; i++) {
        memcpy(buffer + used, "../", 3);
        used += 3;
    }
    size_t tail = strlen(rest);
    while (used + tail < total) {
        buffer[used++] = '/';
    }
    memcpy(buffer + used, rest, tail + 1);
    return buffer;
}

// The padded climbs of `count` `..` to proc/uptime of 4095, 4096 and 4097
// bytes, each looked up.
static void look_up_lengths(const char *where, int count) {
    static char name[2 * PATH_MAX];
    for (size_t total = PATH_MAX - 1; total <= PATH_MAX + 1; total++) {
        char shown[64];
        snprintf(shown, sizeof(shown), "(../ x%d, %zu bytes)proc/uptime", count, total);
        look_up(where, shown, padded(name, count, total, "proc/uptime"), 0);
    }
}

int main(void) {
    if (mkdir("/tmp/live", 0755) != 0 || mkdir("/tmp/dead", 0755) != 0 ||
        mkdir("/tmp/sibling", 0755) != 0) {
        perror("mkdir");
        return 1;
    }
    FILE *file = fopen("/tmp/sibling/file", "w");
    if (file == NULL || fputs("contents\n", file) < 0 || fclose(file) != 0) {
        perror("/tmp/sibling/file");
        return 1;
    }
    look_up("absolute", "/proc/fs", "/proc/fs", 1);
    if (chdir("/tmp/live") != 0) {
        perror("chdir /tmp/live");
        return 1;
    }
    look_up("live", "../sibling", "../sibling", 1);

    if (chdir("/tmp/dead") != 0 || rmdir("/tmp/dead") != 0) {
        perror("chdir and rmdir /tmp/dead");
        return 1;
    }
    print_getcwd("removed");
    open_only("removed", "../../proc/fs", "../../proc/fs", 1);
    look_up("removed", "../../proc", "../../proc", 1);
    look_up("removed", "../../proc/uptime/", "../../proc/uptime/", 0);
    look_up("removed", "/proc/fs", "/proc/fs", 1);
    look_up("removed", "../sibling", "../sibling", 1);
    look_up("removed", "../sibling/file", "../sibling/file", 0);
    look_up("removed", ".", ".", 1);
    look_up("removed", "x", "x", 0);
    look_up_lengths("removed", 2);

    // A working directory 4293 bytes long: /tmp/L, 14 directories of
    // 251 or 252 bytes below it, then s and three more of 251 bytes, 20
    // levels below the root in all. It is entered at /tmp/s/A/A/A, while
    // its path is short, and /tmp/s is then moved below the long path.
    char name[251];
    memset(name, 'a', sizeof(name) - 1);
    name[sizeof(name) - 1] = '\0';
    char deep[PATH_MAX];
    snprintf(deep, sizeof(deep), "/tmp/L");
    if (mkdir(deep, 0755) != 0) {
        perror("mkdir /tmp/L");
        return 1;
    }
    for (int level = 0; level < 14; level++) {
        size_t used = strlen(deep);
        snprintf(deep + used, sizeof(deep) - used, "/%s%d", name, level);
        if (mkdir(deep, 0755) != 0) {
            perror("mkdir below /tmp/L");
            return 1;
        }
    }
    char entered[PATH_MAX];
    snprintf(entered, sizeof(entered), "/tmp/s");
    if (mkdir(entered, 0755) != 0) {
        perror("mkdir /tmp/s");
        return 1;
    }
    for (int level = 0; level < 3; level++) {
        size_t used = strlen(entered);
        snprintf(entered + used, sizeof(entered) - used, "/%s", name);
        if (mkdir(entered, 0755) != 0) {
            perror("mkdir below /tmp/s");
            return 1;
        }
    }
    char inside[2 * PATH_MAX];
    snprintf(inside, sizeof(inside), "%s/proc", entered);
    if (mkdir(inside, 0755) != 0) {
        perror("mkdir proc");
        return 1;
    }
    snprintf(inside, sizeof(inside), "%s/proc/fs", entered);
    if (mkdir(inside, 0755) != 0) {
        perror("mkdir proc/fs");
        return 1;
    }
    snprintf(inside, sizeof(inside), "%s/x", entered);
    if (mkdir(inside, 0755) != 0) {
        perror("mkdir x");
        return 1;
    }
    if (chdir(entered) != 0) {
        perror("chdir");
        return 1;
    }
    char moved[2 * PATH_MAX];
    snprintf(moved, sizeof(moved), "%s/s", deep);
    if (rename("/tmp/s", moved) != 0) {
        perror("rename");
        return 1;
    }
    char spelled[PATH_MAX];
    print_getcwd("overlong");
    look_up("overlong", "proc/fs", "proc/fs", 1);
    look_up("overlong", "x/../proc/fs", "x/../proc/fs", 1);
    look_up("overlong", "(../ x20)proc/fs", climbing(spelled, sizeof(spelled), 20, "proc/fs"), 1);
    look_up("overlong", "(../ x20)proc", climbing(spelled, sizeof(spelled), 20, "proc"), 1);
    look_up("overlong", "(../ x20)proc/uptime/",
            climbing(spelled, sizeof(spelled), 20, "proc/uptime/"), 0);
    look_up("overlong", "(../ x19)sibling", climbing(spelled, sizeof(spelled), 19, "sibling"), 1);
    look_up_lengths("overlong", 20);

    int next = open("/dev/null", O_RDONLY | O_CLOEXEC);
    printf("next descriptor: %d\n", next);
    return 0;
}
