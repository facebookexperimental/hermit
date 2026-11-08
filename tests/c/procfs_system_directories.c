/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* stat, lstat, opendir and the listing of every system-wide /proc directory
 * of Hermit's --proc level table, by several spellings, with the answers
 * Linux gives around them: one line per directory or spelling, which the
 * KVM cli test requires to equal ptrace's line for line at both levels
 * (run_kvm_procfs_system_directories_answer_as_on_ptrace).
 *
 * Usage: procfs_system_directories DIR, where DIR is a writable directory for
 * a symbolic link to /proc/sys. Inode numbers are not printed, only whether
 * the listing's agree with stat's.
 *
 * procfs_system_directories --pair A B: the line of each of the two
 * directories, then whether they are one object, as a bind mount of one on
 * the other makes them: six CHECK lines and a RESULT line that the KVM cli
 * test requires to equal Linux's own, run outside Hermit in user and mount
 * namespaces of its own, as well as ptrace's
 * (run_kvm_procfs_bind_mounts_are_the_directory_they_show). The exit status
 * is 1 when a check fails. */

#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <unistd.h>

static const char *DIRECTORIES[] = {
    "/proc/sys",
    "/proc/sys/kernel",
    "/proc/sys/kernel/random",
    "/proc/sys/vm",
    "/proc/driver",
    "/proc/pressure",
    "/proc/sys/fs",
    "/proc/sys/kernel/pty",
    "/proc/sys/net",
    "/proc/sys/net/core",
    "/proc/sys/net/ipv4",
    "/proc/tty",
};

static const char *errname(void) {
    return strerrorname_np(errno);
}

/* The fields of a stat result that do not depend on inode numbering. */
static void print_stat(const char *label, int rc, const struct stat *st) {
    if (rc < 0) {
        printf(" %s=%s", label, errname());
        return;
    }
    printf(" %s=mode:%o,nlink:%lu,uid:%u,gid:%u,size:%ld,blksize:%ld,blocks:%ld,dev:%lx",
           label, st->st_mode, (unsigned long)st->st_nlink, st->st_uid, st->st_gid,
           (long)st->st_size, (long)st->st_blksize, (long)st->st_blocks,
           (unsigned long)st->st_dev);
}

static int same_stat(const struct stat *a, const struct stat *b) {
    return a->st_mode == b->st_mode && a->st_nlink == b->st_nlink && a->st_uid == b->st_uid &&
           a->st_gid == b->st_gid && a->st_size == b->st_size && a->st_dev == b->st_dev &&
           a->st_ino == b->st_ino;
}

/* Reads the open directory to the end: its names and types, space
 * separated, into `names`, and whether each entry's inode number agrees
 * with stat's of it (`.` with the directory's own, each subdirectory with
 * the subdirectory's). */
static int list(DIR *directory, const char *path, ino_t own, char *names, size_t size,
                int *inodes_agree) {
    size_t used = 0;
    struct dirent *entry;
    errno = 0;
    names[0] = '\0';
    while ((entry = readdir(directory)) != NULL) {
        int n = snprintf(names + used, size - used, "%s%s/%u", used ? " " : "", entry->d_name,
                         entry->d_type);
        if (n < 0 || (size_t)n >= size - used) {
            return -1;
        }
        used += (size_t)n;
        if (strcmp(entry->d_name, ".") == 0) {
            *inodes_agree &= entry->d_ino == own;
        } else if (entry->d_type == DT_DIR && strcmp(entry->d_name, "..") != 0) {
            char child[PATH_MAX];
            struct stat st;
            snprintf(child, sizeof(child), "%s/%s", path, entry->d_name);
            *inodes_agree &= stat(child, &st) == 0 && st.st_ino == entry->d_ino;
        }
    }
    return errno ? -1 : 0;
}

static int compare_names(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

static void directory_line(const char *path) {
    struct stat st, lst, fst;
    int s = stat(path, &st);
    printf("%s:", path);
    print_stat("stat", s, &st);
    int l = lstat(path, &lst);
    print_stat("lstat", l, &lst);
    DIR *directory = opendir(path);
    if (!directory) {
        printf(" opendir=%s", errname());
    } else {
        int f = fstat(dirfd(directory), &fst);
        printf(" opendir=ok fstat=%s", f < 0 ? errname() : (s == 0 && same_stat(&st, &fst)) ? "same" : "differs");
        static char names[16384], again[16384];
        int agree = 1;
        if (list(directory, path, s == 0 ? st.st_ino : 0, names, sizeof(names), &agree) < 0) {
            printf(" readdir=%s", errname());
        } else {
            printf(" readdir=[%s] inodes=%s", names, agree ? "agree" : "differ");
            rewinddir(directory);
            int ignored = 1;
            if (list(directory, path, 0, again, sizeof(again), &ignored) < 0) {
                printf(" rewind=%s", errname());
            } else {
                printf(" rewind=%s", strcmp(names, again) == 0 ? "same" : "differs");
            }
        }
        struct statfs fs;
        if (fstatfs(dirfd(directory), &fs) < 0) {
            printf(" fstatfs=%s", errname());
        } else {
            printf(" fstatfs=%lx", (unsigned long)fs.f_type);
        }
        int dot = openat(dirfd(directory), "..", O_PATH | O_DIRECTORY | O_CLOEXEC);
        printf(" openat(..)=%s", dot < 0 ? errname() : "ok");
        if (dot >= 0) {
            close(dot);
        }
        closedir(directory);
    }
    int tmp = open(path, O_TMPFILE | O_RDWR | O_CLOEXEC, 0600);
    printf(" tmpfile=%s", tmp < 0 ? errname() : "ok");
    if (tmp >= 0) {
        close(tmp);
    }
    printf(" access(W)=%s", faccessat(AT_FDCWD, path, W_OK, 0) < 0 ? errname() : "ok");
    struct statfs fs;
    printf(" statfs=%s\n", statfs(path, &fs) < 0 ? errname() : "ok");
}

/* stat, lstat and opendir of `path`, by a spelling of interest. */
static void spelling_line(const char *label, const char *path) {
    struct stat st;
    printf("%s:", label);
    int s = stat(path, &st);
    printf(" stat=%s", s < 0 ? errname() : S_ISDIR(st.st_mode) ? "directory" : "other");
    int l = lstat(path, &st);
    printf(" lstat=%s", l < 0 ? errname() : S_ISDIR(st.st_mode) ? "directory" : S_ISLNK(st.st_mode) ? "link" : "other");
    DIR *directory = opendir(path);
    printf(" opendir=%s", directory ? "ok" : errname());
    if (directory) {
        static char names[16384];
        int agree = 1;
        if (list(directory, path, s == 0 ? st.st_ino : 0, names, sizeof(names), &agree) == 0) {
            printf(" readdir=[%s]", names);
        }
        closedir(directory);
    }
    int nofollow = open(path, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    printf(" open(O_NOFOLLOW)=%s\n", nofollow < 0 ? errname() : "ok");
    if (nofollow >= 0) {
        close(nofollow);
    }
}

/* The sorted names and types of the directory at `path`, one string, or
 * NULL when it cannot be read. */
static char *sorted_names(const char *path) {
    static char names[16384];
    DIR *directory = opendir(path);
    if (!directory) {
        return NULL;
    }
    int agree = 1;
    int failed = list(directory, path, 0, names, sizeof(names), &agree) < 0;
    closedir(directory);
    if (failed) {
        return NULL;
    }
    char *entries[512];
    size_t count = 0;
    for (char *name = strtok(names, " "); name; name = strtok(NULL, " ")) {
        if (count == sizeof(entries) / sizeof(entries[0])) {
            return NULL;
        }
        entries[count++] = name;
    }
    qsort(entries, count, sizeof(entries[0]), compare_names);
    static char sorted[2][16384];
    static int which;
    char *out = sorted[which ^= 1];
    out[0] = '\0';
    for (size_t i = 0; i < count; i++) {
        strcat(out, i ? " " : "");
        strcat(out, entries[i]);
    }
    return out;
}

static int checks, failures;

static void check(const char *label, int ok) {
    checks++;
    failures += !ok;
    printf("CHECK %s %d\n", label, ok);
}

/* Whether `a` and `b` are one directory: stat, fstat of an open descriptor
 * and the listing of each agree. */
static int pair(const char *a, const char *b) {
    const char *paths[2] = {a, b};
    struct stat st[2], fst[2];
    int rc[2], opened[2];
    for (int i = 0; i < 2; i++) {
        directory_line(paths[i]);
        rc[i] = stat(paths[i], &st[i]);
        int fd = open(paths[i], O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        opened[i] = fd >= 0;
        char label[16];
        snprintf(label, sizeof(label), "fstat_%d", i);
        check(label, fd >= 0 && fstat(fd, &fst[i]) == 0 && rc[i] == 0 &&
                         fst[i].st_dev == st[i].st_dev && fst[i].st_ino == st[i].st_ino &&
                         fst[i].st_mode == st[i].st_mode);
        if (fd >= 0) {
            close(fd);
        }
    }
    check("both_stat_directory",
          rc[0] == 0 && rc[1] == 0 && S_ISDIR(st[0].st_mode) && S_ISDIR(st[1].st_mode));
    check("same_object_stat",
          rc[0] == 0 && rc[1] == 0 && st[0].st_dev == st[1].st_dev && st[0].st_ino == st[1].st_ino);
    check("both_open_directory", opened[0] && opened[1]);
    const char *first = sorted_names(a);
    const char *second = sorted_names(b);
    check("same_object_listing", first && second && strcmp(first, second) == 0);
    printf("RESULT checks=%d failures=%d\n", checks, failures);
    return failures ? 1 : 0;
}

int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "--pair") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        return pair(argv[2], argv[3]);
    }
    if (argc != 2) {
        fprintf(stderr, "usage: %s DIR | --pair A B\n", argv[0]);
        return 2;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    for (size_t i = 0; i < sizeof(DIRECTORIES) / sizeof(DIRECTORIES[0]); i++) {
        directory_line(DIRECTORIES[i]);
    }
    char link[PATH_MAX];
    snprintf(link, sizeof(link), "%s/procfs-system-directories-link", argv[1]);
    unlink(link);
    if (symlink("/proc/sys", link) < 0) {
        perror("symlink");
        return 1;
    }
    char below[PATH_MAX + 16];
    snprintf(below, sizeof(below), "%s/kernel/random", link);
    spelling_line("a link to /proc/sys", link);
    spelling_line("below a link to /proc/sys", below);
    spelling_line("//proc/./sys/", "//proc/./sys/");
    spelling_line("/proc/sys/kernel/../vm", "/proc/sys/kernel/../vm");
    spelling_line("/proc/sys/kernel/no-such-name", "/proc/sys/kernel/no-such-name");
    spelling_line("/proc/sys/crypto", "/proc/sys/crypto");
    /* The longest pathname Linux takes, PATH_MAX - 1 bytes, and one byte
     * longer, which getname refuses before any lookup. */
    static char longest[PATH_MAX + 2];
    strcpy(longest, "/proc/sys");
    while (strlen(longest) + 2 <= PATH_MAX - 1) {
        strcat(longest, "/.");
    }
    printf("(%zu bytes) ", strlen(longest));
    spelling_line("/proc/sys + /.", longest);
    strcat(longest, "/");
    printf("(%zu bytes) ", strlen(longest));
    spelling_line("/proc/sys + /. + /", longest);
    unlink(link);
    return 0;
}
