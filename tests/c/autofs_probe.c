/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Probe for the autofs tests: report each operation's outcome as
// "<op> <path>: OK" or "<op> <path>: errno=<n>". Modes:
//   autofs_probe stat PATH...
//   autofs_probe lstat PATH...
//   autofs_probe opath PATH...        (open O_PATH)
//   autofs_probe opathdir PATH...     (open O_PATH|O_DIRECTORY)
//   autofs_probe statx PATH...        (statx, automount allowed)
//   autofs_probe readdir PATH         (sorted names, comma-joined)
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int cmp_str(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: autofs_probe MODE PATH...\n");
        return 2;
    }
    const char *mode = argv[1];
    for (int i = 2; i < argc; i++) {
        const char *path = argv[i];
        errno = 0;
        if (strcmp(mode, "stat") == 0 || strcmp(mode, "lstat") == 0) {
            struct stat st;
            int rc = strcmp(mode, "stat") == 0 ? stat(path, &st) : lstat(path, &st);
            if (rc == 0)
                printf("%s %s: OK dev=%llx ino=%lld\n", mode, path,
                       (unsigned long long)st.st_dev, (long long)st.st_ino);
            else
                printf("%s %s: errno=%d\n", mode, path, errno);
        } else if (strcmp(mode, "opath") == 0 || strcmp(mode, "opathdir") == 0) {
            int flags = O_PATH | O_CLOEXEC;
            if (strcmp(mode, "opathdir") == 0) flags |= O_DIRECTORY;
            int fd = open(path, flags);
            if (fd >= 0) {
                printf("%s %s: OK\n", mode, path);
                close(fd);
            } else {
                printf("%s %s: errno=%d\n", mode, path, errno);
            }
        } else if (strcmp(mode, "statx") == 0) {
            struct statx stx;
            long rc = syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BASIC_STATS, &stx);
            if (rc == 0)
                printf("statx %s: OK mnt_id=%lld ino=%lld\n", path,
                       (long long)stx.stx_mnt_id, (long long)stx.stx_ino);
            else
                printf("statx %s: errno=%d\n", path, errno);
        } else if (strcmp(mode, "readdir") == 0) {
            DIR *d = opendir(path);
            if (!d) {
                printf("readdir %s: errno=%d\n", path, errno);
                continue;
            }
            char **names = NULL;
            size_t n = 0, cap = 0;
            struct dirent *e;
            while ((e = readdir(d)) != NULL) {
                if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
                if (n == cap) {
                    cap = cap ? cap * 2 : 16;
                    names = realloc(names, cap * sizeof *names);
                }
                names[n++] = strdup(e->d_name);
            }
            closedir(d);
            qsort(names, n, sizeof *names, cmp_str);
            printf("readdir %s: OK [", path);
            for (size_t k = 0; k < n; k++) printf("%s%s", k ? "," : "", names[k]);
            printf("]\n");
        } else {
            fprintf(stderr, "unknown mode %s\n", mode);
            return 2;
        }
    }
    return 0;
}
