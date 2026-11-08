/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* readlink and readlinkat of the system-wide procfs directories by many
 * spellings, each answer printed as an errno name or the link's length and
 * bytes, so that native Linux and a Hermit backend can be compared line by
 * line.
 *
 *   procfs_system_directory_links DIR   spellings; DIR is a writable
 *                                       directory for a symbolic link to
 *                                       /proc/sys
 *   procfs_system_directory_links --bind
 *                                       the two names a bind over /proc/sys
 *                                       holding kernel -> /dev/null changes
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void show(const char *what, ssize_t length, const char *buffer) {
    if (length < 0) {
        printf(" %s=%s", what, strerrorname_np(errno));
    } else {
        printf(" %s=%zd:%.*s", what, length, (int)length, buffer);
    }
}

static void both(const char *label, const char *path) {
    char buffer[256];
    printf("%s:", label);
    show("readlink", readlink(path, buffer, sizeof(buffer)), buffer);
    show("readlinkat", readlinkat(AT_FDCWD, path, buffer, sizeof(buffer)), buffer);
    printf("\n");
}

static void relative(const char *label, int dirfd, const char *path) {
    char buffer[256];
    printf("%s:", label);
    show("readlinkat", readlinkat(dirfd, path, buffer, sizeof(buffer)), buffer);
    printf("\n");
}

/* "/proc/", then `slashes` slashes, then "sys"; or "/proc/sys" padded with
 * "/." to `length` bytes. */
static char *spelling(size_t slashes, size_t length) {
    char *path = calloc(1, slashes + length + 16);
    if (path == NULL) {
        exit(2);
    }
    if (length == 0) {
        strcpy(path, "/proc/");
        memset(path + 6, '/', slashes);
        strcpy(path + 6 + slashes, "sys");
    } else {
        strcpy(path, "/proc/sys");
        while (strlen(path) + 2 <= length) {
            strcat(path, "/.");
        }
    }
    return path;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s DIR | --bind\n", argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "--bind") == 0) {
        both("/proc/sys/kernel", "/proc/sys/kernel");
        both("/proc/sys", "/proc/sys");
        return 0;
    }
    both("/proc/sys", "/proc/sys");
    both("/proc/sys/", "/proc/sys/");
    both("//proc/sys", "//proc/sys");
    both("/proc/./sys", "/proc/./sys");
    both("/proc/sys/kernel/random", "/proc/sys/kernel/random");
    both("/proc/sys/kernel/random/uuid", "/proc/sys/kernel/random/uuid");
    both("/proc/sys/hermit-absent", "/proc/sys/hermit-absent");
    char *overlong = spelling(4096, 0);
    printf("(%zu bytes) ", strlen(overlong));
    both("/proc/ + slashes + sys", overlong);
    char *longest = spelling(0, 4095);
    printf("(%zu bytes) ", strlen(longest));
    both("/proc/sys + /. padding", longest);
    int proc = open("/proc", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (proc < 0) {
        perror("open /proc");
        return 1;
    }
    relative("sys from a /proc descriptor", proc, "sys");
    relative("sys/kernel from a /proc descriptor", proc, "sys/kernel");
    char link[4096];
    snprintf(link, sizeof(link), "%s/to-sys", argv[1]);
    if (symlink("/proc/sys", link) != 0) {
        perror("symlink");
        return 1;
    }
    both("DIR/to-sys", link);
    strcat(link, "/kernel");
    both("DIR/to-sys/kernel", link);
    free(overlong);
    free(longest);
    close(proc);
    return 0;
}
