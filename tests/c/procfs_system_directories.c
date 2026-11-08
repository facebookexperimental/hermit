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
 * is 1 when a check fails.
 *
 * procfs_system_directories --open-flags DIR: eight legacy open and openat
 * calls of DIR, a directory on a read-only procfs bind: O_PATH with
 * O_TMPFILE, with and without O_RDWR, opens the directory path-only (Linux
 * keeps only O_PATH_FLAGS); O_TMPFILE|O_RDWR is EROFS; O_TMPFILE|O_RDONLY
 * is EINVAL.
 *
 * procfs_system_directories --overmount ALIAS SOURCE: ALIAS holds a bind
 * of the sysctl directory SOURCE, over which another directory was then
 * mounted; ALIAS must still report the procfs directory's own metadata
 * (mode 0555, one link, size 0), by stat and fstat, and list its entries
 * and not the other directory's foreign_marker.
 *
 * Both print CHECK lines and a RESULT line and exit 1 when a check fails;
 * the KVM cli test requires them to pass and to equal ptrace's
 * (run_kvm_procfs_bind_mount_objects_answer_as_on_ptrace). */

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
#include <sys/syscall.h>
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

/* The eight legacy opens of `--open-flags`. */
static int open_flags(const char *path) {
    const struct {
        const char *name;
        int flags;
        int expected_errno;
    } shapes[] = {
        {"path_tmpfile", O_PATH | O_TMPFILE, 0},
        {"path_tmpfile_rdwr", O_PATH | O_TMPFILE | O_RDWR, 0},
        {"tmpfile_rdwr", O_TMPFILE | O_RDWR, EROFS},
        {"tmpfile_rdonly", O_TMPFILE | O_RDONLY, EINVAL},
    };
    for (int at = 0; at < 2; at++) {
        for (size_t i = 0; i < sizeof(shapes) / sizeof(shapes[0]); i++) {
            errno = 0;
            long fd = at ? syscall(SYS_openat, AT_FDCWD, path, shapes[i].flags, 0600)
                         : syscall(SYS_open, path, shapes[i].flags, 0600);
            int call_errno = fd < 0 ? errno : 0;
            int directory = 0, path_only = 0, no_create = 0, read_only = 0;
            if (fd >= 0) {
                struct stat st;
                directory = fstat((int)fd, &st) == 0 && S_ISDIR(st.st_mode);
                int status = fcntl((int)fd, F_GETFL);
                if (status >= 0) {
                    path_only = (status & O_PATH) != 0;
                    no_create = (status & (O_TMPFILE & ~O_DIRECTORY)) == 0;
                    read_only = (status & O_ACCMODE) == O_RDONLY;
                }
                close((int)fd);
            }
            int ok = shapes[i].expected_errno == 0
                         ? fd >= 0 && directory && path_only && no_create && read_only
                         : fd < 0 && call_errno == shapes[i].expected_errno;
            char label[64];
            snprintf(label, sizeof(label), "%s_%s errno=%s", at ? "openat" : "open",
                     shapes[i].name, call_errno ? strerrorname_np(call_errno) : "0");
            check(label, ok);
        }
    }
    printf("RESULT checks=%d failures=%d\n", checks, failures);
    return failures ? 1 : 0;
}

/* The ten checks of `--overmount`. */
static int overmount(const char *alias, const char *source) {
    struct stat st, fst, covered;
    struct statfs fs, covered_fs;
    int a = stat(alias, &st), af = statfs(alias, &fs);
    int b = stat(source, &covered), bf = statfs(source, &covered_fs);
    printf("OBJECT %s mode=%o nlink=%lu size=%ld blocks=%ld\n", alias,
           a == 0 ? st.st_mode & 07777 : 0, a == 0 ? (unsigned long)st.st_nlink : 0,
           a == 0 ? (long)st.st_size : -1, a == 0 ? (long)st.st_blocks : -1);
    check("alias_is_procfs_directory", a == 0 && S_ISDIR(st.st_mode) && af == 0 && fs.f_type == 0x9fa0);
    check("source_is_now_foreign", b == 0 && S_ISDIR(covered.st_mode) && bf == 0 && covered_fs.f_type != 0x9fa0);
    check("mode_is_the_objects", a == 0 && (st.st_mode & 07777) == 0555);
    check("links_are_the_objects", a == 0 && st.st_nlink == 1);
    check("size_is_the_objects", a == 0 && st.st_size == 0);
    int fd = open(alias, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    check("open_alias", fd >= 0);
    check("fstat_is_the_objects", fd >= 0 && fstat(fd, &fst) == 0 && S_ISDIR(fst.st_mode) &&
                                      (fst.st_mode & 07777) == 0555 && fst.st_nlink == 1 &&
                                      fst.st_size == 0);
    if (fd >= 0) {
        close(fd);
    }
    DIR *directory = opendir(alias);
    check("list_alias", directory != NULL);
    int wanted = 0, marker = 0;
    if (directory) {
        struct dirent *entry;
        while ((entry = readdir(directory)) != NULL) {
            wanted += strcmp(entry->d_name, "max_map_count") == 0 ||
                      strcmp(entry->d_name, "mmap_min_addr") == 0 ||
                      strcmp(entry->d_name, "overcommit_memory") == 0;
            marker += strcmp(entry->d_name, "foreign_marker") == 0;
        }
        closedir(directory);
    }
    check("lists_the_objects_entries", wanted == 3);
    check("does_not_list_the_covering_directory", marker == 0);
    printf("RESULT checks=%d failures=%d\n", checks, failures);
    return failures ? 1 : 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "--open-flags") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        return open_flags(argv[2]);
    }
    if (argc == 4 && strcmp(argv[1], "--overmount") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        return overmount(argv[2], argv[3]);
    }
    if (argc == 4 && strcmp(argv[1], "--pair") == 0) {
        setvbuf(stdout, NULL, _IOLBF, 0);
        return pair(argv[2], argv[3]);
    }
    if (argc != 2) {
        fprintf(stderr, "usage: %s DIR | --pair A B | --open-flags DIR | --overmount ALIAS SOURCE\n",
                argv[0]);
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
