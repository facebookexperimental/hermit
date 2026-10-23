/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* This is an observation probe, not a determinization implementation. Every
 * byte string is hex encoded, including file and directory names. A failed or
 * incomplete observation is a record that the conformance harness must reject.
 * Raw namespace identities require --no-virtualize-metadata. Normal metadata
 * runs use the same probe and Hermit's unchanged strict verification policy.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#define OBSERVATION_LIMIT (16U * 1024U * 1024U)

static const char *const namespace_names[] = {
    "user", "pid", "uts", "mnt", "net", "ipc", "cgroup"
};

static void hex(const unsigned char *bytes, size_t size) {
    static const char digits[] = "0123456789abcdef";
    for (size_t i = 0; i < size; ++i) {
        putchar(digits[bytes[i] >> 4]);
        putchar(digits[bytes[i] & 15]);
    }
}

static void string(const char *value) {
    hex((const unsigned char *)value, strlen(value));
}

static void stat_path(const char *tag, const char *path) {
    struct stat st = {0};
    errno = 0;
    int rc = stat(path, &st);
    int error = rc < 0 ? errno : 0;
    printf("STAT %s rc=%d errno=%d dev=%llu ino=%llu mode=%o nlink=%llu\n",
           tag, rc, error, (unsigned long long)st.st_dev,
           (unsigned long long)st.st_ino, st.st_mode,
           (unsigned long long)st.st_nlink);
}

static void link_path(const char *tag, const char *path) {
    unsigned char bytes[PATH_MAX + 1];
    errno = 0;
    ssize_t size = readlink(path, (char *)bytes, sizeof(bytes));
    int error = size < 0 ? errno : 0;
    int complete = size < (ssize_t)sizeof(bytes);
    printf("LINK %s rc=%ld errno=%d complete=%d bytes=", tag, (long)size, error, complete);
    if (size >= 0) hex(bytes, (size_t)size);
    putchar('\n');
}

static void filesystem(const char *tag, const char *path) {
    stat_path(tag, path);
    struct statfs st = {0};
    errno = 0;
    int rc = statfs(path, &st);
    int error = rc < 0 ? errno : 0;
    printf("STATFS %s rc=%d errno=%d type=%lx flags=%lx\n",
           tag, rc, error, (unsigned long)st.f_type, (unsigned long)st.f_flags);
}

static void file(const char *tag, const char *path) {
    errno = 0;
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        printf("FILE %s rc=-1 errno=%d complete=0 bytes=\n", tag, errno);
        return;
    }
    unsigned char *bytes = malloc(OBSERVATION_LIMIT + 1);
    if (!bytes) {
        printf("FILE %s rc=-1 errno=%d complete=0 bytes=\n", tag, ENOMEM);
        close(fd);
        return;
    }
    size_t used = 0;
    ssize_t count = 0;
    do {
        size_t request = OBSERVATION_LIMIT + 1 - used;
        if (request > 65536) request = 65536;
        count = read(fd, bytes + used, request);
        if (count > 0) used += (size_t)count;
    } while (count > 0 && used <= OBSERVATION_LIMIT);
    int error = count < 0 ? errno : 0;
    int complete = error == 0 && used <= OBSERVATION_LIMIT;
    printf("FILE %s rc=%ld errno=%d complete=%d bytes=", tag,
           count < 0 ? -1L : (long)used, error, complete);
    /* Never emit a prefix as though it were a complete observation. */
    if (complete) hex(bytes, used);
    putchar('\n');
    free(bytes);
    close(fd);
}

static void directory(const char *tag, const char *path) {
    struct dirent **entries = NULL;
    errno = 0;
    int count = scandir(path, &entries, NULL, alphasort);
    int error = count < 0 ? errno : 0;
    printf("DIRECTORY %s rc=%d errno=%d\n", tag, count, error);
    if (count < 0) return;
    for (int i = 0; i < count; ++i) {
        if (strcmp(entries[i]->d_name, ".") && strcmp(entries[i]->d_name, "..")) {
            printf("ENTRY %s bytes=", tag);
            string(entries[i]->d_name);
            putchar('\n');
        }
        free(entries[i]);
    }
    free(entries);
}

static void capability(const char *tag, const char *path) {
    errno = 0;
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    int error = fd < 0 ? errno : 0;
    struct stat object = {0};
    errno = 0;
    int stat_rc = fd < 0 ? -1 : fstat(fd, &object);
    int stat_error = stat_rc < 0 ? (fd < 0 ? error : errno) : 0;
    printf("CAPABILITY %s reachable=%d errno=%d stat_rc=%d stat_errno=%d dev=%llu ino=%llu\n",
           tag, fd >= 0, error, stat_rc, stat_error,
           (unsigned long long)object.st_dev, (unsigned long long)object.st_ino);
    if (fd >= 0) close(fd);
}

#define DESCRIPTOR_REQUEST_LIMIT 4096U
#define DESCRIPTOR_ARGUMENT_LIMIT 65536U

struct descriptor_request {
    char role[32];
    int fd;
    int attempted;
};
static struct descriptor_request descriptor_requests[DESCRIPTOR_REQUEST_LIMIT];
static size_t descriptor_request_count;
static int task_one_probed;

static int decimal(const char *value, int positive, int *number) {
    if (!value[0] || (value[0] == '0' && value[1])) return -1;
    unsigned int result = 0;
    for (const unsigned char *p = (const unsigned char *)value; *p; ++p) {
        if (*p < '0' || *p > '9' || result > (INT_MAX - (*p - '0')) / 10U) return -1;
        result = result * 10U + (*p - '0');
    }
    if (positive && !result) return -1;
    *number = (int)result;
    return 0;
}

/* The host authenticates the complete prior capture. These argv keys only
 * request real operations; they do not certify a process or inventory. */
static int descriptor_arguments(int argc, char **argv) {
    size_t bytes = 0;
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--descriptor-obligation")) continue;
        if (++i == argc) return -1;
        const char *value = argv[i];
        const char *slash = strrchr(value, '/');
        if (!slash || slash == value || !slash[1]) return -1;
        size_t length = (size_t)(slash - value);
        if (length >= sizeof(descriptor_requests[0].role)) return -1;
        char role[32] = {0};
        memcpy(role, value, length);
        if (strcmp(role, "self") && strcmp(role, "pid1")) {
            int tid;
            if (strncmp(role, "pid1_task/", 10) || decimal(role + 10, 1, &tid)) return -1;
        }
        int fd;
        if (decimal(slash + 1, 0, &fd)) return -1;
        if (descriptor_request_count == DESCRIPTOR_REQUEST_LIMIT ||
            strlen(value) + 1 > DESCRIPTOR_ARGUMENT_LIMIT - bytes) return -1;
        bytes += strlen(value) + 1;
        for (size_t n = 0; n < descriptor_request_count; ++n) {
            if (descriptor_requests[n].fd == fd && !strcmp(descriptor_requests[n].role, role)) return -1;
        }
        struct descriptor_request *request = &descriptor_requests[descriptor_request_count++];
        memcpy(request->role, role, length + 1);
        request->fd = fd;
    }
    return 0;
}

static void descriptor_objects(const char *tag, const char *base) {
    char path[PATH_MAX], key[128];
    struct stat st = {0};
    snprintf(path, sizeof(path), "%s/fd", base);
    errno = 0;
    int rc = stat(path, &st);
    int error = rc < 0 ? errno : 0;
    printf("FD_PARENT %s rc=%d errno=%d dev=%llu ino=%llu mode=%o\n",
           tag, rc, error, (unsigned long long)st.st_dev,
           (unsigned long long)st.st_ino, st.st_mode);
    for (int fd = 42; fd <= 44; ++fd) {
        snprintf(key, sizeof(key), "%s/%d", tag, fd);
        snprintf(path, sizeof(path), "%s/fd/%d", base, fd);
        memset(&st, 0, sizeof(st));
        errno = 0;
        rc = stat(path, &st);
        error = rc < 0 ? errno : 0;
        printf("FD_PROC_OBJECT %s rc=%d errno=%d dev=%llu ino=%llu mode=%o\n",
               key, rc, error, (unsigned long long)st.st_dev,
               (unsigned long long)st.st_ino, st.st_mode);
        if (!strcmp(tag, "self")) {
            memset(&st, 0, sizeof(st));
            errno = 0;
            rc = fstat(fd, &st);
            error = rc < 0 ? errno : 0;
            printf("FD_SELF_OBJECT %s rc=%d errno=%d dev=%llu ino=%llu mode=%o\n",
                   key, rc, error, (unsigned long long)st.st_dev,
                   (unsigned long long)st.st_ino, st.st_mode);
        }
    }
}

static void descriptor_marker(const char *tag, const char *base, long fd) {
    char path[PATH_MAX], key[128];
    snprintf(key, sizeof(key), "%s/%ld", tag, fd);
    if (!strcmp(tag, "self")) {
        errno = 0;
        int marker = openat((int)fd, "descriptor-marker", O_RDONLY | O_CLOEXEC);
        int marker_error = marker < 0 ? errno : 0;
        struct stat marker_object = {0};
        errno = 0;
        int marker_stat = marker < 0 ? -1 : fstat(marker, &marker_object);
        int marker_stat_error = marker_stat < 0 ? (marker < 0 ? marker_error : errno) : 0;
        printf("FD_OPENAT %s reachable=%d errno=%d stat_rc=%d stat_errno=%d dev=%llu ino=%llu\n",
               key, marker >= 0, marker_error, marker_stat, marker_stat_error,
               (unsigned long long)marker_object.st_dev, (unsigned long long)marker_object.st_ino);
        if (marker >= 0) close(marker);
    }
    snprintf(path, sizeof(path), "%s/fd/%ld/descriptor-marker", base, fd);
    capability(key, path);
}

static void descriptor_probe(const char *tag, const char *base, long fd) {
    char path[PATH_MAX], key[128];
    snprintf(key, sizeof(key), "%s/%ld", tag, fd);
    snprintf(path, sizeof(path), "%s/fd/%ld", base, fd);
    stat_path(key, path);
    link_path(key, path);
    snprintf(path, sizeof(path), "%s/fdinfo/%ld", base, fd);
    file(key, path);
    if (!strcmp(tag, "self")) {
        errno = 0;
        int flags = fcntl((int)fd, F_GETFD);
        int flags_error = flags < 0 ? errno : 0;
        errno = 0;
        int mode = fcntl((int)fd, F_GETFL);
        int mode_error = mode < 0 ? errno : 0;
        printf("FD_FLAGS %s flags=%d flags_errno=%d mode=%d mode_errno=%d\n",
               key, flags, flags_error, mode, mode_error);
    }
    descriptor_marker(tag, base, fd);
    for (size_t n = 0; n < descriptor_request_count; ++n) {
        if (descriptor_requests[n].fd == fd && !strcmp(descriptor_requests[n].role, tag))
            descriptor_requests[n].attempted = 1;
    }
}

static int listed_descriptor(const long *numbers, size_t used, long fd) {
    for (size_t n = 0; n < used; ++n) if (numbers[n] == fd) return 1;
    return 0;
}

static void additional_descriptors(const char *tag, const char *base,
                                   const long *numbers, size_t used) {
    for (size_t n = 0; n < descriptor_request_count; ++n) {
        struct descriptor_request *request = &descriptor_requests[n];
        if (!strcmp(request->role, tag) && !request->attempted)
            descriptor_probe(tag, base, request->fd);
    }
    for (int fd = 42; fd <= 44; ++fd) {
        int requested = 0;
        for (size_t n = 0; n < descriptor_request_count; ++n) {
            if (descriptor_requests[n].fd == fd && !strcmp(descriptor_requests[n].role, tag))
                requested = 1;
        }
        if (!requested && !listed_descriptor(numbers, used, fd))
            descriptor_marker(tag, base, fd);
    }
}

static void descriptors(const char *tag, const char *base) {
    char path[PATH_MAX];
    if (!strcmp(tag, "pid1_task/1")) task_one_probed = 1;
    descriptor_objects(tag, base);
    snprintf(path, sizeof(path), "%s/fd", base);
    errno = 0;
    DIR *stream = opendir(path);
    int error = stream ? 0 : errno;
    if (!stream) {
        printf("FD_DIRECTORY %s rc=-1 errno=%d\n", tag, error);
        additional_descriptors(tag, base, NULL, 0);
        return;
    }
    long numbers[4096];
    size_t used = 0;
    int count = 0;
    for (;;) {
        errno = 0;
        struct dirent *entry = readdir(stream);
        if (!entry) { error = errno; break; }
        ++count;
        if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) continue;
        char *end = NULL;
        errno = 0;
        long fd = strtol(entry->d_name, &end, 10);
        if (errno || !entry->d_name[0] || !end || *end || fd < 0 || fd > INT_MAX) { error = EINVAL; break; }
        if (used == sizeof(numbers) / sizeof(numbers[0])) { error = EOVERFLOW; break; }
        numbers[used++] = fd;
    }
    printf("FD_DIRECTORY %s rc=%d errno=%d\n", tag, error ? -1 : count, error);
    printf("FD_SCANNER %s fd=%d\n", tag, dirfd(stream));
    if (error) {
        closedir(stream);
        additional_descriptors(tag, base, NULL, 0);
        return;
    }
    /* Keep the scanner alive until every listed descriptor is probed. Its
     * actual descriptor number and object remain part of the full surface. */
    for (size_t i = 0; i < used; ++i) {
        long fd = numbers[i];
        printf("FD_ENTRY %s fd=%ld\n", tag, fd);
    }
    for (size_t i = 0; i < used; ++i) {
        if (!listed_descriptor(numbers, i, numbers[i]))
            descriptor_probe(tag, base, numbers[i]);
    }
    /* A missing numeric entry never suppresses a requested operation. */
    additional_descriptors(tag, base, numbers, used);
    errno = 0;
    int closed = closedir(stream);
    printf("FD_SCAN_COMPLETE %s rc=%d errno=%d\n", tag, closed, closed < 0 ? errno : 0);
}

static void process(const char *tag, const char *base) {
    char path[PATH_MAX], key[128];
    snprintf(path, sizeof(path), "%s/status", base);
    snprintf(key, sizeof(key), "%s/status", tag);
    file(key, path);
    for (size_t i = 0; i < sizeof(namespace_names) / sizeof(namespace_names[0]); ++i) {
        snprintf(path, sizeof(path), "%s/ns/%s", base, namespace_names[i]);
        snprintf(key, sizeof(key), "%s/ns/%s", tag, namespace_names[i]);
        stat_path(key, path);
        link_path(key, path);
    }
    descriptors(tag, base);
    const char *const views[] = {"cwd", "root"};
    for (size_t i = 0; i < sizeof(views) / sizeof(views[0]); ++i) {
        snprintf(path, sizeof(path), "%s/%s", base, views[i]);
        snprintf(key, sizeof(key), "%s/%s", tag, views[i]);
        stat_path(key, path);
        link_path(key, path);
    }
}

int main(int argc, char **argv) {
    if (descriptor_arguments(argc, argv)) return 2;
    int barrier = 0;
    const char *metadata_mode = "raw";
    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--observer-barrier")) barrier = 1;
        if (!strcmp(argv[i], "--normal-metadata")) metadata_mode = "normal";
    }
    printf("SCHEMA backend-isolation-v1 metadata=%s\n", metadata_mode);
    for (int i = 1; i + 3 < argc; ++i) {
        if (!strcmp(argv[i], "--host-ns")) {
            printf("HOST_NS %s dev=%s ino=%s\n", argv[i + 1], argv[i + 2], argv[i + 3]);
            i += 3;
        }
    }
    for (int i = 1; i + 2 < argc; ++i) {
        if (!strcmp(argv[i], "--host-tmp"))
            printf("HOST_TMP tmp dev=%s ino=%s\n", argv[i + 1], argv[i + 2]);
        if (!strcmp(argv[i], "--host-directory"))
            printf("HOST_DIRECTORY sentinel dev=%s ino=%s\n", argv[i + 1], argv[i + 2]);
        if (!strcmp(argv[i], "--host-file"))
            printf("HOST_FILE sentinel dev=%s ino=%s\n", argv[i + 1], argv[i + 2]);
        if (!strcmp(argv[i], "--host-socket-object"))
            printf("HOST_SOCKET sentinel dev=%s ino=%s\n", argv[i + 1], argv[i + 2]);
    }
    printf("IDENTITY self getpid=%ld getppid=%ld\n", (long)getpid(), (long)getppid());
    /* An actual rewriting site keeps the e9patch cell from being a no-op
     * backend selector. Its value is not an isolation observation. */
    unsigned int cycle_low, cycle_high;
    __asm__ volatile("rdtsc" : "=a"(cycle_low), "=d"(cycle_high));
    (void)cycle_low;
    (void)cycle_high;
    puts("PREPROCESS_SITE rdtsc executed=1");
    char bytes[PATH_MAX] = {0};
    errno = 0;
    int hostname_rc = gethostname(bytes, sizeof(bytes));
    int hostname_error = hostname_rc < 0 ? errno : 0;
    printf("HOSTNAME self rc=%d errno=%d bytes=", hostname_rc, hostname_error);
    if (hostname_rc == 0) string(bytes);
    putchar('\n');
    errno = 0;
    char *cwd = getcwd(bytes, sizeof(bytes));
    int cwd_error = cwd ? 0 : errno;
    printf("CWD self rc=%d errno=%d bytes=", cwd ? 0 : -1, cwd_error);
    if (cwd) string(cwd);
    putchar('\n');

    process("self", "/proc/self");
    process("pid1", "/proc/1");
    directory("pid1_tasks", "/proc/1/task");
    struct dirent **tasks = NULL;
    int task_count = scandir("/proc/1/task", &tasks, NULL, alphasort);
    if (task_count >= 0) {
        for (int i = 0; i < task_count; ++i) {
            char *end = NULL;
            long tid = strtol(tasks[i]->d_name, &end, 10);
            if (tasks[i]->d_name[0] && end && !*end && tid > 0) {
                char path[128], key[128];
                snprintf(path, sizeof(path), "/proc/1/task/%ld", tid);
                snprintf(key, sizeof(key), "pid1_task/%ld", tid);
                process(key, path);
            }
            free(tasks[i]);
        }
        free(tasks);
    }
    /* Seeded task1 and prior tasks remain attempted when today's task list is
     * unavailable or different. Canonical paths are operations, not role proof. */
    if (!task_one_probed) descriptors("pid1_task/1", "/proc/1/task/1");
    for (size_t n = 0; n < descriptor_request_count; ++n) {
        struct descriptor_request *request = &descriptor_requests[n];
        if (request->attempted) continue;
        char path[128];
        if (!strcmp(request->role, "self")) snprintf(path, sizeof(path), "/proc/self");
        else if (!strcmp(request->role, "pid1")) snprintf(path, sizeof(path), "/proc/1");
        else snprintf(path, sizeof(path), "/proc/1/task/%s", request->role + 10);
        descriptors(request->role, path);
    }
    const char *const names[] = {"root", "tmp", "proc", "sys", "dev", "cwd", "shm", "devpts", "cgroup"};
    const char *const paths[] = {"/", "/tmp", "/proc", "/sys", "/dev", ".", "/dev/shm", "/dev/pts", "/sys/fs/cgroup"};
    for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); ++i) filesystem(names[i], paths[i]);
    file("mountinfo", "/proc/self/mountinfo");
    file("mounts", "/proc/self/mounts");
    file("kernel_hostname", "/proc/sys/kernel/hostname");
    file("net_dev", "/proc/net/dev");
    file("net_route", "/proc/net/route");
    file("lo_flags", "/sys/class/net/lo/flags");
    file("cgroup", "/proc/self/cgroup");
    file("sysvipc_shm", "/proc/sysvipc/shm");
    directory("tmp", "/tmp");
    directory("cwd", ".");
    directory("interfaces", "/sys/class/net");
    directory("dev", "/dev");
    directory("shm", "/dev/shm");
    directory("cgroup", "/sys/fs/cgroup");
    capability("self_fd42", "/proc/self/fd/42/descriptor-marker");
    capability("pid1_fd42", "/proc/1/fd/42/descriptor-marker");
    capability("pid1_thread_fd42", "/proc/1/task/1/fd/42/descriptor-marker");
    for (int i = 1; i + 1 < argc; ++i) {
        if (!strcmp(argv[i], "--host-ipc")) {
            struct shmid_ds information;
            errno = 0;
            int rc = shmctl(atoi(argv[i + 1]), IPC_STAT, &information);
            printf("SENTINEL ipc reachable=%d errno=%d\n", rc == 0, rc < 0 ? errno : 0);
        }
        if (!strcmp(argv[i], "--host-socket")) {
            struct sockaddr_un address = { .sun_family = AF_UNIX };
            size_t length = strlen(argv[i + 1]);
            if (length >= sizeof(address.sun_path) - 1) return 2;
            memcpy(address.sun_path + 1, argv[i + 1], length);
            errno = 0;
            int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
            int rc = fd < 0 ? -1 : connect(fd, (struct sockaddr *)&address,
                (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + length));
            int error = rc < 0 ? errno : 0;
            printf("SENTINEL socket reachable=%d errno=%d\n", rc == 0, error);
            if (fd >= 0) close(fd);
        }
        if (!strcmp(argv[i], "--host-pid")) {
            char path[128];
            snprintf(path, sizeof(path), "/proc/%s/stat", argv[i + 1]);
            file("host_pid_stat", path);
            snprintf(path, sizeof(path), "/proc/%s/ns/pid", argv[i + 1]);
            stat_path("host_pid_namespace", path);
        }
    }
    if (barrier) {
        /* The native observer supplies the same authorized stdin byte for
         * every physical run. This holds the real process alive without a
         * clock/scheduler shortcut and is not an isolation capability grant.
         */
        unsigned char acknowledgment = 0;
        puts("BARRIER_READY stdin");
        fflush(stdout);
        errno = 0;
        ssize_t count = read(STDIN_FILENO, &acknowledgment, 1);
        int error = count < 0 ? errno : 0;
        printf("BARRIER stdin rc=%ld errno=%d byte=%u\n", (long)count, error, acknowledgment);
    }
    puts("COMPLETE backend-isolation-v1");
    return ferror(stdout) ? 2 : 0;
}
