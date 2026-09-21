/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/magic.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <unistd.h>

static bool is_proc_mount_table(const char *path) {
    if (path == NULL) {
        return false;
    }
    if (strcmp(path, "/proc/mounts") == 0 ||
        strcmp(path, "/proc/self/mounts") == 0 ||
        strcmp(path, "/proc/thread-self/mounts") == 0 ||
        strcmp(path, "/proc/self/mountinfo") == 0 ||
        strcmp(path, "/proc/thread-self/mountinfo") == 0) {
        return true;
    }

    const char *cursor = path;
    if (strncmp(cursor, "/proc/", 6) != 0) {
        return false;
    }
    cursor += 6;
    if (*cursor < '0' || *cursor > '9') {
        return false;
    }
    while (*cursor >= '0' && *cursor <= '9') {
        ++cursor;
    }
    return strcmp(cursor, "/mounts") == 0 ||
           strcmp(cursor, "/mountinfo") == 0;
}

static int fixed_mount_fd(void) {
    const char *value = getenv("HERMIT_LSOF_MOUNTS_FD");
    char *end = NULL;
    long parsed;
    struct stat source_stat;
    struct statfs source_fs;

    if (value == NULL || *value == '\0') {
        errno = EBADF;
        return -1;
    }
    errno = 0;
    parsed = strtol(value, &end, 10);
    if (errno != 0 || end == value || *end != '\0' || parsed < 0 ||
        parsed > INT_MAX) {
        errno = EBADF;
        return -1;
    }
    if (fstat((int)parsed, &source_stat) != 0 ||
        fstatfs((int)parsed, &source_fs) != 0) {
        return -1;
    }
    if (!S_ISREG(source_stat.st_mode) || source_fs.f_type == PROC_SUPER_MAGIC) {
        errno = EPERM;
        return -1;
    }
    if (lseek((int)parsed, 0, SEEK_SET) < 0) {
        return -1;
    }
    return (int)parsed;
}

static void mark_redirect(void) {
    const char *marker = getenv("HERMIT_LSOF_REDIRECT_MARKER");
    static const char message[] = "fixed-mount-fd\n";

    if (marker == NULL) {
        return;
    }
    int fd = (int)syscall(
        SYS_openat,
        AT_FDCWD,
        marker,
        O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC,
        0600);
    if (fd >= 0) {
        (void)write(fd, message, sizeof(message) - 1);
        (void)close(fd);
    }
}

static int duplicate_mount_fd(void) {
    int source = fixed_mount_fd();
    if (source < 0) {
        return -1;
    }
    return fcntl(source, F_DUPFD_CLOEXEC, 3);
}

FILE *fopen64(const char *path, const char *mode) {
    static FILE *(*next_fopen64)(const char *, const char *);
    if (next_fopen64 == NULL) {
        next_fopen64 = dlsym(RTLD_NEXT, "fopen64");
    }
    if (!is_proc_mount_table(path)) {
        return next_fopen64(path, mode);
    }

    int fd = duplicate_mount_fd();
    if (fd < 0) {
        return NULL;
    }
    FILE *stream = fdopen(fd, mode);
    if (stream == NULL) {
        (void)close(fd);
    } else {
        mark_redirect();
    }
    return stream;
}

FILE *fopen(const char *path, const char *mode) {
    static FILE *(*next_fopen)(const char *, const char *);
    if (next_fopen == NULL) {
        next_fopen = dlsym(RTLD_NEXT, "fopen");
    }
    if (!is_proc_mount_table(path)) {
        return next_fopen(path, mode);
    }

    int fd = duplicate_mount_fd();
    if (fd < 0) {
        return NULL;
    }
    FILE *stream = fdopen(fd, mode);
    if (stream == NULL) {
        (void)close(fd);
    } else {
        mark_redirect();
    }
    return stream;
}

int open(const char *path, int flags, ...) {
    static int (*next_open)(const char *, int, ...);
    mode_t mode = 0;
    if (next_open == NULL) {
        next_open = dlsym(RTLD_NEXT, "open");
    }
    if (is_proc_mount_table(path)) {
        int fd = duplicate_mount_fd();
        if (fd >= 0) {
            mark_redirect();
        }
        return fd;
    }
    if ((flags & O_CREAT) != 0 || (flags & O_TMPFILE) == O_TMPFILE) {
        va_list args;
        va_start(args, flags);
        mode = va_arg(args, mode_t);
        va_end(args);
        return next_open(path, flags, mode);
    }
    return next_open(path, flags);
}

int open64(const char *path, int flags, ...) {
    static int (*next_open64)(const char *, int, ...);
    mode_t mode = 0;
    if (next_open64 == NULL) {
        next_open64 = dlsym(RTLD_NEXT, "open64");
    }
    if (is_proc_mount_table(path)) {
        int fd = duplicate_mount_fd();
        if (fd >= 0) {
            mark_redirect();
        }
        return fd;
    }
    if ((flags & O_CREAT) != 0 || (flags & O_TMPFILE) == O_TMPFILE) {
        va_list args;
        va_start(args, flags);
        mode = va_arg(args, mode_t);
        va_end(args);
        return next_open64(path, flags, mode);
    }
    return next_open64(path, flags);
}

int openat(int dirfd, const char *path, int flags, ...) {
    static int (*next_openat)(int, const char *, int, ...);
    mode_t mode = 0;
    if (next_openat == NULL) {
        next_openat = dlsym(RTLD_NEXT, "openat");
    }
    if (is_proc_mount_table(path)) {
        int fd = duplicate_mount_fd();
        if (fd >= 0) {
            mark_redirect();
        }
        return fd;
    }
    if ((flags & O_CREAT) != 0 || (flags & O_TMPFILE) == O_TMPFILE) {
        va_list args;
        va_start(args, flags);
        mode = va_arg(args, mode_t);
        va_end(args);
        return next_openat(dirfd, path, flags, mode);
    }
    return next_openat(dirfd, path, flags);
}
