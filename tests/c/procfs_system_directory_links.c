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
 *   procfs_system_directory_links DIR [--native-settle]
 *                                       spellings; DIR is a writable
 *                                       directory for symbolic links to
 *                                       /proc/sys, one direct and one at
 *                                       the end of a chain of 39
 *   procfs_system_directory_links --bind
 *                                       the two names a bind over /proc/sys
 *                                       holding kernel -> /dev/null changes
 *   procfs_system_directory_links --magic DIR
 *                                       spellings through the caller's own
 *                                       /proc/self/cwd magic link
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* --native-settle, for the native reference run only: an ELOOP is asked
 * again, up to 8 times, and each answer is followed by " settle=N", the
 * attempts it took. Linux keeps a lookup's link count across the restart
 * of its RCU walk (set_nameidata once, then path_lookupat again; fs/namei.c
 * v6.12), which a mount anywhere on the host forces, so a native lookup near
 * MAXSYMLINKS that crosses procfs's self can fail with ELOOP below the
 * budget; it never succeeds past it. A run under Hermit asks each call
 * once. */
static bool native_settle;

static void show(const char *what, ssize_t length, const char *buffer, int attempts) {
    if (length < 0) {
        printf(" %s=%s", what, strerrorname_np(errno));
    } else {
        printf(" %s=%zd:%.*s", what, length, (int)length, buffer);
    }
    if (native_settle) {
        printf(" settle=%d", attempts);
    }
}

static void both(const char *label, const char *path) {
    char buffer[256];
    printf("%s:", label);
    for (int at = 0; at <= 1; at++) {
        ssize_t length = -1;
        int attempts = 0;
        do {
            attempts++;
            length = at ? readlinkat(AT_FDCWD, path, buffer, sizeof(buffer))
                        : readlink(path, buffer, sizeof(buffer));
        } while (native_settle && length < 0 && errno == ELOOP && attempts < 8);
        show(at ? "readlinkat" : "readlink", length, buffer, attempts);
    }
    printf("\n");
}

static void relative(const char *label, int dirfd, const char *path) {
    char buffer[256];
    printf("%s:", label);
    show("readlinkat", readlinkat(dirfd, path, buffer, sizeof(buffer)), buffer, 1);
    printf("\n");
}

/* `prefix`, then `real` (a path with no symbolic link in it), then
 * "/chainN/": the final "/" makes the lookup follow the last link too.
 * `sized` prints the pathname's length first, for one that is the same
 * length natively and under Hermit. */
static void chained(const char *label, const char *prefix, const char *real, int first,
                    bool sized) {
    char path[2 * PATH_MAX];
    snprintf(path, sizeof(path), "%s%s/chain%d/", prefix, real, first);
    if (sized) {
        printf("(%zu bytes) ", strlen(path));
    }
    both(label, path);
}

/* DIR/chain0 -> chain1 -> ... -> chain38 -> /proc/sys: 39 links. */
static int make_chain(const char *dir) {
    for (int index = 0; index <= 38; index++) {
        char name[2 * PATH_MAX];
        char target[32];
        snprintf(name, sizeof(name), "%s/chain%d", dir, index);
        if (index == 38) {
            strcpy(target, "/proc/sys");
        } else {
            snprintf(target, sizeof(target), "chain%d", index + 1);
        }
        if (symlink(target, name) != 0) {
            perror("symlink chain");
            return -1;
        }
    }
    return 0;
}

/* Below `real`, directories whose names make `prefix` + the deep directory
 * + "/chain0/" exactly `total` bytes long; the deep directory in `deep`. */
static int make_deep(const char *prefix, const char *real, size_t total, char *deep) {
    strcpy(deep, real);
    size_t room = total - strlen(prefix) - strlen(real) - strlen("/chain0/");
    size_t count = (room + 255) / 256;
    size_t letters = room - count;
    for (size_t index = 0; index < count; index++) {
        size_t length = letters / count + (index < letters % count ? 1 : 0);
        size_t end = strlen(deep);
        deep[end] = '/';
        memset(deep + end + 1, 'd', length);
        deep[end + 1 + length] = '\0';
        if (mkdir(deep, 0755) != 0) {
            perror("mkdir deep");
            return -1;
        }
    }
    return make_chain(deep);
}

/* Magic links of the caller's own task: /proc/self/cwd with the working
 * directory a fixture holding kernel -> /proc/sys, then /proc/sys itself. */
static int magic(const char *dir) {
    char link[PATH_MAX];
    snprintf(link, sizeof(link), "%s/kernel", dir);
    if (symlink("/proc/sys", link) != 0 || chdir(dir) != 0) {
        perror("magic fixture");
        return 1;
    }
    both("cwd=F: /proc/self/cwd/kernel", "/proc/self/cwd/kernel");
    both("cwd=F: /proc/self/cwd/kernel/", "/proc/self/cwd/kernel/");
    both("cwd=F: /proc/self/cwd/kernel/..", "/proc/self/cwd/kernel/..");
    printf("chdir /proc/sys: %s\n", chdir("/proc/sys") == 0 ? "ok" : strerrorname_np(errno));
    both("cwd=/proc/sys: /proc/self/cwd/", "/proc/self/cwd/");
    both("cwd=/proc/sys: /proc/self/cwd/kernel", "/proc/self/cwd/kernel");
    both("cwd=/proc/sys: /proc/thread-self/cwd/kernel", "/proc/thread-self/cwd/kernel");
    return 0;
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
    if (argc == 3 && strcmp(argv[1], "--magic") == 0) {
        return magic(argv[2]);
    }
    if (argc == 3 && strcmp(argv[2], "--native-settle") == 0) {
        native_settle = true;
        argc = 2;
    }
    if (argc != 2) {
        fprintf(stderr, "usage: %s DIR [--native-settle] | --bind | --magic DIR\n", argv[0]);
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

    /* One MAXSYMLINKS (40) budget covers the whole lookup. DIR/chain0 ->
     * chain1 -> ... -> chain38 -> /proc/sys is 39 links, chainN/ is 39 - N;
     * /proc/self/fd/N spends 2 (self and the descriptor's link) and each
     * /proc/self/../.. 1 (self). Up to 40 links reach /proc/sys, no link:
     * EINVAL. 41 is ELOOP (fs/namei.c, reserve_stack). */
    char real[PATH_MAX];
    if (realpath(argv[1], real) == NULL) {
        perror("realpath");
        return 1;
    }
    if (make_chain(real) != 0) {
        return 1;
    }
    char through[64];
    snprintf(through, sizeof(through), "/proc/self/fd/%d/..", proc);
    char through_self[128];
    snprintf(through_self, sizeof(through_self), "%s/proc/self/../..", through);
    chained("chain, 39 links", "", real, 0, false);
    chained("/proc/self/fd/N/.. + chain, 39 links", through, real, 2, false);
    chained("/proc/self/fd/N/.. + chain, 40 links", through, real, 1, false);
    chained("/proc/self/fd/N/.. + chain, 41 links", through, real, 0, false);
    chained("/proc/self/../.. + chain, 40 links", "/proc/self/../..", real, 0, false);
    chained("/proc/self/../.. x2 + chain, 41 links",
            "/proc/self/../../proc/self/../..", real, 0, false);
    chained("/proc/self/fd/N/../proc/self/../.. + chain, 40 links", through_self, real, 2, false);
    chained("/proc/self/fd/N/../proc/self/../.. + chain, 41 links", through_self, real, 1, false);

    /* The longest pathname Linux takes, PATH_MAX - 1 bytes, through the
     * descriptor: Detcore's walk adds nothing to it. */
    char deep[2 * PATH_MAX];
    if (make_deep(through, real, PATH_MAX - 1, deep) != 0) {
        return 1;
    }
    chained("deep /proc/self/fd/N/.. + chain, 39 links", through, deep, 2, true);
    chained("deep /proc/self/fd/N/.. + chain, 40 links", through, deep, 1, true);
    chained("deep /proc/self/fd/N/.. + chain, 41 links", through, deep, 0, true);

    /* Names that only look like procfs's: a fixture's own proc/self/cwd, a
     * symbolic link to /proc/sys. */
    char fixture[2 * PATH_MAX];
    snprintf(fixture, sizeof(fixture), "%s/fixture", real);
    if (mkdir(fixture, 0755) != 0 || (strcat(fixture, "/proc"), mkdir(fixture, 0755)) != 0 ||
        (strcat(fixture, "/self"), mkdir(fixture, 0755)) != 0 ||
        (strcat(fixture, "/cwd"), symlink("/proc/sys", fixture)) != 0) {
        perror("fixture");
        return 1;
    }
    strcat(fixture, "/kernel");
    both("DIR/fixture/proc/self/cwd/kernel", fixture);

    /* ".." of the directory a symbolic link reaches, not of the link's
     * spelling: DIR/to-random/../random is /proc/sys/kernel/random. */
    char to_random[2 * PATH_MAX];
    snprintf(to_random, sizeof(to_random), "%s/to-random", real);
    if (symlink("/proc/sys/kernel/random", to_random) != 0) {
        perror("symlink to-random");
        return 1;
    }
    strcat(to_random, "/../random");
    both("DIR/to-random/../random", to_random);
    both("/proc/self/../sys", "/proc/self/../sys");
    both("/proc/thread-self/../../../sys", "/proc/thread-self/../../../sys");
    free(overlong);
    free(longest);
    close(proc);
    return 0;
}
