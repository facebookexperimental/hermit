/*
 * An inode identifies an open object, not its descriptor-table slot. Exercise
 * ordinary files and pipes installed at fds 0–2, plus their duplicates.
 * descriptor-reuse checks descriptors, ordinary paths, fdinfo and pipe links.
 * complete-stat-routes additionally checks followed and no-follow proc-fd stat.
 * Every assertion in a selected mode is mandatory; there is no fallback mode.
 * Inherited stdio uses only fstat bookkeeping: its cross-entrypoint alias
 * behavior is a separate repair. Each mode applies unchanged on native Linux,
 * ptrace and KVM.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <unistd.h>

static int report_fd = STDERR_FILENO;

#define CHECK(condition)                                                       \
    do {                                                                       \
        if (!(condition)) {                                                    \
            dprintf(report_fd, "line %d: %s (errno=%d)\n", __LINE__,             \
                    #condition, errno);                                        \
            exit(1);                                                           \
        }                                                                      \
    } while (0)

static int same_object(struct stat left, struct stat right) {
    return left.st_dev == right.st_dev && left.st_ino == right.st_ino;
}

static void check_statx_identity(struct statx actual, struct stat expected) {
    CHECK((actual.stx_mask & (STATX_INO | STATX_TYPE)) ==
          (STATX_INO | STATX_TYPE));
    CHECK(actual.stx_ino == expected.st_ino);
    CHECK(actual.stx_dev_major == major(expected.st_dev));
    CHECK(actual.stx_dev_minor == minor(expected.st_dev));
    CHECK(((mode_t)actual.stx_mode & S_IFMT) == (expected.st_mode & S_IFMT));
}

static void check_descriptor_stat_routes(int fd, struct stat expected) {
    const int flags[] = {AT_EMPTY_PATH, AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW};
    for (size_t i = 0; i < sizeof(flags) / sizeof(flags[0]); ++i) {
        struct stat actual;
        CHECK(syscall(SYS_newfstatat, fd, "", &actual, flags[i]) == 0);
        CHECK(same_object(actual, expected));
        struct statx extended;
        CHECK(syscall(SYS_statx, fd, "", flags[i], STATX_BASIC_STATS,
                      &extended) == 0);
        check_statx_identity(extended, expected);
    }

    /* Linux copies the pathname before writing a possibly overlapping output.
     * An observer must capture its empty-path decision before that write too. */
    union {
        struct stat result;
        char path[sizeof(struct stat)];
    } overlapping = {0};
    CHECK(syscall(SYS_newfstatat, fd, overlapping.path, &overlapping.result,
                  AT_EMPTY_PATH) == 0);
    CHECK(same_object(overlapping.result, expected));
    union {
        struct statx result;
        char path[sizeof(struct statx)];
    } overlapping_x = {0};
    CHECK(syscall(SYS_statx, fd, overlapping_x.path, AT_EMPTY_PATH,
                  STATX_BASIC_STATS, &overlapping_x.result) == 0);
    check_statx_identity(overlapping_x.result, expected);
}

static struct stat stat_fd(int fd) {
    struct stat result;
    /* Keep inherited-stream bookkeeping on its original fstat entrypoint. */
    CHECK(syscall(SYS_fstat, fd, &result) == 0);
    return result;
}

static struct stat stat_ordinary_descriptor(int fd) {
    struct stat expected = stat_fd(fd);
    check_descriptor_stat_routes(fd, expected);
    return expected;
}

static struct stat stat_complete_routes(int fd) {
    struct stat expected = stat_ordinary_descriptor(fd);

    /* The proc-fd magic link follows this same ordinary object. Its no-follow
     * view is a symlink and must not acquire the target's inode or file type. */
    char path[64];
    CHECK(snprintf(path, sizeof(path), "/proc/self/fd/%d", fd) > 0);
    struct stat followed, link;
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, path, &followed, 0) == 0);
    CHECK(same_object(followed, expected));
    struct statx extended;
    CHECK(syscall(SYS_statx, AT_FDCWD, path, 0, STATX_BASIC_STATS,
                  &extended) == 0);
    check_statx_identity(extended, expected);
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, path, &link,
                  AT_SYMLINK_NOFOLLOW) == 0);
    CHECK(S_ISLNK(link.st_mode));
    CHECK(syscall(SYS_statx, AT_FDCWD, path, AT_SYMLINK_NOFOLLOW,
                  STATX_BASIC_STATS, &extended) == 0);
    check_statx_identity(extended, link);
    return expected;
}

static void check_stat_errors(int valid_fd) {
    struct stat result;
    struct statx extended;
    errno = 0;
    CHECK(syscall(SYS_newfstatat, valid_fd, "", &result, 0) == -1 &&
          errno == ENOENT);
    errno = 0;
    CHECK(syscall(SYS_statx, valid_fd, "", 0, STATX_BASIC_STATS, &extended) == -1 &&
          errno == ENOENT);
    errno = 0;
    CHECK(syscall(SYS_newfstatat, -1, "", &result, AT_EMPTY_PATH) == -1 &&
          errno == EBADF);
    errno = 0;
    CHECK(syscall(SYS_statx, -1, "", AT_EMPTY_PATH, STATX_BASIC_STATS,
                  &extended) == -1 && errno == EBADF);
    errno = 0;
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, ".", &result,
                  AT_EMPTY_PATH | 0x40000000) == -1 && errno == EINVAL);
    errno = 0;
    CHECK(syscall(SYS_statx, AT_FDCWD, ".", AT_EMPTY_PATH | 0x40000000,
                  STATX_BASIC_STATS, &extended) == -1 && errno == EINVAL);
    errno = 0;
    CHECK(syscall(SYS_newfstatat, valid_fd, (char *)1, &result,
                  AT_EMPTY_PATH) == -1 && errno == EFAULT);
    errno = 0;
    CHECK(syscall(SYS_statx, valid_fd, (char *)1, AT_EMPTY_PATH,
                  STATX_BASIC_STATS, &extended) == -1 && errno == EFAULT);

    /* AT_FDCWD with an empty path is the directory, never an inherited stream. */
    struct stat cwd;
    CHECK(syscall(SYS_newfstatat, AT_FDCWD, ".", &cwd, 0) == 0);
    check_descriptor_stat_routes(AT_FDCWD, cwd);
}

static void check_fdinfo(int fd, struct stat expected) {
    char path[64];
    CHECK(snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd) > 0);
    int info = open(path, O_RDONLY | O_CLOEXEC);
    CHECK(info >= 0);
    char text[4096];
    ssize_t size = read(info, text, sizeof(text) - 1);
    CHECK(size > 0 && size < (ssize_t)sizeof(text) - 1);
    CHECK(close(info) == 0);
    text[size] = '\0';
    const char *line = strstr(text, "\nino:\t");
    CHECK(line != NULL);
    char *end;
    unsigned long long inode = strtoull(line + strlen("\nino:\t"), &end, 10);
    CHECK(*end == '\n' && inode == (unsigned long long)expected.st_ino);
}

static void check_pipe_link(int fd, struct stat expected) {
    char path[64], text[128], wanted[128];
    CHECK(snprintf(path, sizeof(path), "/proc/self/fd/%d", fd) > 0);
    ssize_t size = readlink(path, text, sizeof(text) - 1);
    CHECK(size > 0 && size < (ssize_t)sizeof(text) - 1);
    text[size] = '\0';
    CHECK(snprintf(wanted, sizeof(wanted), "pipe:[%llu]",
                   (unsigned long long)expected.st_ino) > 0);
    CHECK(strcmp(text, wanted) == 0);
}

typedef struct stat (*OrdinaryStat)(int fd);

static void check_reuse(const int saved[3], const struct stat initial[3],
                        OrdinaryStat stat_object) {
    char path_a[] = "stdio-inode-a-XXXXXX";
    char path_b[] = "stdio-inode-b-XXXXXX";
    int file_a = mkstemp(path_a), file_b = mkstemp(path_b);
    CHECK(file_a >= 3 && file_b >= 3);
    struct stat identity_a = stat_object(file_a);
    struct stat identity_b = stat_object(file_b);
    CHECK(identity_a.st_dev == identity_b.st_dev);
    CHECK(identity_a.st_ino != identity_b.st_ino);

    /* AT_EMPTY_PATH does not turn a nonempty pathname into a descriptor stat. */
    char cwd[3072], absolute_a[4096];
    CHECK(getcwd(cwd, sizeof(cwd)) != NULL);
    int path_size = snprintf(absolute_a, sizeof(absolute_a), "%s/%s", cwd, path_a);
    CHECK(path_size > 0 && (size_t)path_size < sizeof(absolute_a));
    struct stat absolute_stat;
    CHECK(syscall(SYS_newfstatat, saved[1], absolute_a, &absolute_stat,
                  AT_EMPTY_PATH) == 0);
    CHECK(same_object(absolute_stat, identity_a));
    struct statx absolute_statx;
    CHECK(syscall(SYS_statx, saved[1], absolute_a, AT_EMPTY_PATH,
                  STATX_BASIC_STATS, &absolute_statx) == 0);
    check_statx_identity(absolute_statx, identity_a);

    for (int low = 0; low < 3; ++low) {
        CHECK(close(low) == 0);
        CHECK(open(path_a, O_RDWR) == low);
        struct stat opened_a = stat_object(low), path_stat;
        CHECK(same_object(opened_a, identity_a));
        CHECK(syscall(SYS_newfstatat, AT_FDCWD, path_a, &path_stat, 0) == 0);
        CHECK(same_object(opened_a, path_stat));
        int alias = dup(low);
        CHECK(alias >= 3);
        CHECK(same_object(opened_a, stat_object(alias)));
        check_fdinfo(low, opened_a);
        check_fdinfo(alias, opened_a);

        CHECK(close(low) == 0);
        CHECK(open(path_b, O_RDWR) == low);
        struct stat opened_b = stat_object(low);
        CHECK(same_object(opened_b, identity_b));
        CHECK(!same_object(opened_a, opened_b));
        CHECK(same_object(opened_a, stat_object(alias)));
        check_fdinfo(low, opened_b);
        CHECK(close(alias) == 0);
        CHECK(dup2(saved[low], low) == low);
        CHECK(same_object(initial[low], stat_fd(low)));
    }

    for (int low = 0; low < 3; ++low) {
        CHECK(close(low) == 0);
        int pipe_fds[2];
        CHECK(pipe(pipe_fds) == 0 && pipe_fds[0] == low);
        struct stat pipe_identity = stat_object(pipe_fds[0]);
        int pipe_alias = dup(pipe_fds[0]);
        CHECK(pipe_alias >= 3);
        CHECK(same_object(pipe_identity, stat_object(pipe_alias)));
        check_fdinfo(pipe_fds[0], pipe_identity);
        check_fdinfo(pipe_alias, pipe_identity);
        check_pipe_link(pipe_fds[0], pipe_identity);
        check_pipe_link(pipe_alias, pipe_identity);
        CHECK(close(pipe_alias) == 0);
        CHECK(close(pipe_fds[0]) == 0);
        CHECK(close(pipe_fds[1]) == 0);
        CHECK(dup2(saved[low], low) == low);
        CHECK(same_object(initial[low], stat_fd(low)));
    }

    CHECK(close(file_a) == 0 && close(file_b) == 0);
    CHECK(unlink(path_a) == 0 && unlink(path_b) == 0);
}

int main(int argc, char **argv) {
    CHECK(argc == 2);
    OrdinaryStat stat_object = NULL;
    if (strcmp(argv[1], "descriptor-reuse") == 0) {
        stat_object = stat_ordinary_descriptor;
    } else if (strcmp(argv[1], "complete-stat-routes") == 0) {
        stat_object = stat_complete_routes;
    }
    CHECK(stat_object != NULL);
    int saved[3];
    struct stat initial[3];
    for (int fd = 0; fd < 3; ++fd) {
        initial[fd] = stat_fd(fd);
        saved[fd] = fcntl(fd, F_DUPFD_CLOEXEC, 10);
        CHECK(saved[fd] >= 10);
    }
    report_fd = saved[2];
    check_stat_errors(saved[1]);
    check_reuse(saved, initial, stat_object);
    for (int fd = 0; fd < 3; ++fd) {
        CHECK(close(saved[fd]) == 0);
    }
    report_fd = STDERR_FILENO;
    CHECK(printf("stdio-inode-%s-ok\n", argv[1]) > 0);
    return 0;
}
