/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Native, independent isolation evidence. This helper observes only its own
 * baseline and the CLI descendant tree. It never reads environments, guest
 * descriptor contents, or unrelated process arguments. All paths and byte
 * strings in the report are hex encoded. Observation limits refuse rather
 * than presenting a truncated prefix as complete evidence. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define FILE_LIMIT (16U * 1024U * 1024U)
#define REPORT_LIMIT (32L * 1024L * 1024L)
#define DESCENDANT_LIMIT 256
#define SNAPSHOT_LIMIT 512

static const char *const namespace_names[] = {
    "user", "pid", "uts", "mnt", "net", "ipc", "cgroup"
};
struct process { pid_t pid, parent; unsigned long long start; };
struct identity { pid_t pid; unsigned long long start; ino_t namespaces[7]; char executable[PATH_MAX]; };
static struct identity identities[SNAPSHOT_LIMIT];
static size_t identity_count, snapshot_count;
static unsigned int observer_errors;
struct owned_process { struct process identity; int retired; };
static struct owned_process owned[DESCENDANT_LIMIT];
static size_t owned_count;
static char guest_name[16], carrier_name[16], observed_role[128];
static int children_unavailable;

static void hex(FILE *output, const unsigned char *bytes, size_t size) {
    static const char digits[] = "0123456789abcdef";
    for (size_t i = 0; i < size; ++i) {
        fputc(digits[bytes[i] >> 4], output);
        fputc(digits[bytes[i] & 15], output);
    }
}

static void error(FILE *output, const char *tag, int code) {
    fprintf(output, "OBSERVER_ERROR %s errno=%d\n", tag, code);
    ++observer_errors;
}

static void file(FILE *output, const char *tag, const char *path) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        fprintf(output, "FILE %s rc=-1 errno=%d complete=0 bytes=\n", tag, errno);
        return;
    }
    unsigned char *bytes = malloc(FILE_LIMIT + 1);
    if (!bytes) { close(fd); error(output, "allocation", ENOMEM); return; }
    size_t used = 0;
    ssize_t count;
    do {
        size_t remaining = FILE_LIMIT + 1 - used;
        count = read(fd, bytes + used, remaining < 65536 ? remaining : 65536);
        if (count > 0) used += (size_t)count;
    } while (count > 0 && used <= FILE_LIMIT);
    int code = count < 0 ? errno : 0;
    int complete = code == 0 && used <= FILE_LIMIT;
    fprintf(output, "FILE %s rc=%ld errno=%d complete=%d bytes=", tag,
            count < 0 ? -1L : (long)used, code, complete);
    if (complete) hex(output, bytes, used);
    fputc('\n', output);
    if (!complete) error(output, "file-incomplete", code ? code : EOVERFLOW);
    free(bytes);
    close(fd);
}

static void stat_path(FILE *output, const char *tag, const char *path) {
    struct stat st = {0};
    int rc = stat(path, &st);
    fprintf(output, "STAT %s rc=%d errno=%d dev=%llu ino=%llu mode=%o nlink=%llu\n",
            tag, rc, rc < 0 ? errno : 0, (unsigned long long)st.st_dev,
            (unsigned long long)st.st_ino, st.st_mode, (unsigned long long)st.st_nlink);
}

static void link_path(FILE *output, const char *tag, const char *path) {
    unsigned char bytes[PATH_MAX + 1];
    ssize_t size = readlink(path, (char *)bytes, sizeof(bytes));
    fprintf(output, "LINK %s rc=%ld errno=%d complete=%d bytes=", tag, (long)size,
            size < 0 ? errno : 0, size < (ssize_t)sizeof(bytes));
    if (size >= 0) hex(output, bytes, (size_t)size);
    fputc('\n', output);
}

static void directory_view(FILE *output, const char *tag, const char *path) {
    struct dirent **entries = NULL;
    int count = scandir(path, &entries, NULL, alphasort);
    fprintf(output, "DIRECTORY %s rc=%d errno=%d\n", tag, count, count < 0 ? errno : 0);
    if (count < 0) return;
    for (int i = 0; i < count; ++i) {
        if (strcmp(entries[i]->d_name, ".") && strcmp(entries[i]->d_name, "..")) {
            fprintf(output, "ENTRY %s bytes=", tag);
            hex(output, (unsigned char *)entries[i]->d_name, strlen(entries[i]->d_name));
            fputc('\n', output);
        }
        free(entries[i]);
    }
    free(entries);
}

static int process_identity(pid_t pid, struct process *process) {
    char path[128], bytes[8192];
    snprintf(path, sizeof(path), "/proc/%ld/stat", (long)pid);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    ssize_t size = read(fd, bytes, sizeof(bytes) - 1);
    close(fd);
    if (size < 0 || size == (ssize_t)sizeof(bytes) - 1) return -1;
    bytes[size] = 0;
    char *tail = strrchr(bytes, ')');
    if (!tail) return -1;
    /* Fields after comm begin with state (field 3); starttime is field 22. */
    char *save = NULL;
    char *token = strtok_r(tail + 1, " ", &save);
    long parent = 0;
    unsigned long long start = 0;
    for (unsigned field = 3; token && field <= 22; ++field) {
        if (field == 4) parent = strtol(token, NULL, 10);
        if (field == 22) start = strtoull(token, NULL, 10);
        token = strtok_r(NULL, " ", &save);
    }
    if (!start) return -1;
    *process = (struct process){pid, (pid_t)parent, start};
    return 0;
}

static void descriptors(FILE *output, const char *tag, const char *base) {
    char path[PATH_MAX], key[256];
    snprintf(path, sizeof(path), "%s/fd", base);
    errno = 0;
    DIR *stream = opendir(path);
    int code = stream ? 0 : errno;
    if (!stream) { fprintf(output, "FD_DIRECTORY %s rc=-1 errno=%d\n", tag, code); return; }
    long numbers[4096];
    size_t used = 0;
    int count = 0;
    for (;;) {
        errno = 0;
        struct dirent *entry = readdir(stream);
        if (!entry) { code = errno; break; }
        ++count;
        if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) continue;
        char *end = NULL;
        errno = 0;
        long fd = strtol(entry->d_name, &end, 10);
        if (errno || !entry->d_name[0] || !end || *end || fd < 0 || fd > INT_MAX) { code = EINVAL; break; }
        if (used == sizeof(numbers) / sizeof(numbers[0])) { code = EOVERFLOW; break; }
        numbers[used++] = fd;
    }
    fprintf(output, "FD_DIRECTORY %s rc=%d errno=%d\n", tag, code ? -1 : count, code);
    fprintf(output, "FD_SCANNER %s fd=%d\n", tag, dirfd(stream));
    if (code) { closedir(stream); error(output, "fd-enumeration", code); return; }
    for (size_t i = 0; i < used; ++i) fprintf(output, "FD_ENTRY %s fd=%ld\n", tag, numbers[i]);
    for (size_t i = 0; i < used; ++i) {
            long fd = numbers[i];
            snprintf(key, sizeof(key), "%s/fd/%ld", tag, fd);
            snprintf(path, sizeof(path), "%s/fd/%ld", base, fd);
            stat_path(output, key, path);
            link_path(output, key, path);
            snprintf(path, sizeof(path), "%s/fdinfo/%ld", base, fd);
            file(output, key, path);
            snprintf(path, sizeof(path), "%s/fd/%ld/descriptor-marker", base, fd);
            int marker = open(path, O_RDONLY | O_CLOEXEC);
            int marker_error = marker < 0 ? errno : 0;
            struct stat object = {0};
            int stat_rc = marker < 0 ? -1 : fstat(marker, &object);
            int stat_error = stat_rc < 0 ? (marker < 0 ? marker_error : errno) : 0;
            fprintf(output, "CAPABILITY %s reachable=%d errno=%d stat_rc=%d stat_errno=%d dev=%llu ino=%llu\n",
                    key, marker >= 0, marker_error, stat_rc, stat_error,
                    (unsigned long long)object.st_dev, (unsigned long long)object.st_ino);
            if (marker >= 0) close(marker);
    }
    errno = 0;
    int closed = closedir(stream);
    fprintf(output, "FD_SCAN_COMPLETE %s rc=%d errno=%d\n", tag, closed, closed < 0 ? errno : 0);
    if (closed < 0) error(output, "fd-enumeration-close", errno);
}

static void observe(FILE *output, const struct process *process, int force) {
    char path[PATH_MAX], executable[PATH_MAX] = {0}, tag[128], key[256];
    struct identity identity = { .pid = process->pid, .start = process->start };
    snprintf(path, sizeof(path), "/proc/%ld/exe", (long)process->pid);
    ssize_t size = readlink(path, executable, sizeof(executable) - 1);
    if (size < 0) return; /* A transient exit is retained by lifecycle rows. */
    memcpy(identity.executable, executable, (size_t)size);
    for (size_t i = 0; i < 7; ++i) {
        struct stat st;
        snprintf(path, sizeof(path), "/proc/%ld/ns/%s", (long)process->pid, namespace_names[i]);
        if (stat(path, &st) == 0) identity.namespaces[i] = st.st_ino;
    }
    size_t generation = identity_count;
    for (size_t i = 0; i < identity_count; ++i) {
        if (identities[i].pid == identity.pid && identities[i].start == identity.start &&
            !strcmp(identities[i].executable, identity.executable) &&
            !memcmp(identities[i].namespaces, identity.namespaces, sizeof(identity.namespaces))) {
            generation = i;
            break;
        }
    }
    if (generation < identity_count && !force) return;
    if (generation == identity_count) {
        if (identity_count == SNAPSHOT_LIMIT) { error(output, "identity-cap", EOVERFLOW); return; }
        identities[identity_count++] = identity;
    }
    if (snapshot_count == SNAPSHOT_LIMIT || ftell(output) >= REPORT_LIMIT) {
        error(output, "snapshot-report-cap", EOVERFLOW);
        return;
    }
    ++snapshot_count;
    snprintf(tag, sizeof(tag), "process/%ld/%zu/%zu", (long)process->pid, generation, snapshot_count);
    fprintf(output, "PROCESS %s pid=%ld parent=%ld start=%llu exe=", tag,
            (long)process->pid, (long)process->parent, process->start);
    hex(output, (unsigned char *)executable, (size_t)size);
    fputc('\n', output);
    for (size_t i = 0; i < 7; ++i) {
        snprintf(path, sizeof(path), "/proc/%ld/ns/%s", (long)process->pid, namespace_names[i]);
        snprintf(key, sizeof(key), "%s/ns/%s", tag, namespace_names[i]);
        stat_path(output, key, path);
    }
    snprintf(path, sizeof(path), "/proc/%ld/status", (long)process->pid);
    snprintf(key, sizeof(key), "%s/status", tag);
    file(output, key, path);
    char comm[16] = {0};
    long innermost_pid = 0;
    FILE *status = fopen(path, "re");
    if (status) {
        char line[8192];
        while (fgets(line, sizeof(line), status)) {
            if (!strncmp(line, "Name:\t", 6)) {
                size_t length = strcspn(line + 6, "\n");
                if (length >= sizeof(comm)) length = sizeof(comm) - 1;
                memcpy(comm, line + 6, length);
            }
            if (!strncmp(line, "NSpid:\t", 7)) {
                char *next = line + 7, *end = NULL;
                while (*next) {
                    while (*next == ' ' || *next == '\t') ++next;
                    long number = strtol(next, &end, 10);
                    if (end == next) break;
                    innermost_pid = number;
                    next = end;
                }
            }
        }
        fclose(status);
    }
    snprintf(path, sizeof(path), "/proc/%ld/mountinfo", (long)process->pid);
    snprintf(key, sizeof(key), "%s/mountinfo", tag);
    file(output, key, path);
    snprintf(path, sizeof(path), "/proc/%ld", (long)process->pid);
    descriptors(output, tag, path);
    const char *const views[] = {"cwd", "root"};
    for (size_t i = 0; i < 2; ++i) {
        snprintf(path, sizeof(path), "/proc/%ld/%s", (long)process->pid, views[i]);
        snprintf(key, sizeof(key), "%s/%s", tag, views[i]);
        stat_path(output, key, path);
        link_path(output, key, path);
    }
    /* Thread-specific proc aliases can retain capabilities after leader exec. */
    snprintf(path, sizeof(path), "/proc/%ld/task", (long)process->pid);
    struct dirent **tasks = NULL;
    int count = scandir(path, &tasks, NULL, alphasort);
    fprintf(output, "TASK_DIRECTORY %s rc=%d errno=%d\n", tag, count, count < 0 ? errno : 0);
    if (count >= 0) {
        for (int i = 0; i < count; ++i) {
            char *end = NULL;
            long tid = strtol(tasks[i]->d_name, &end, 10);
            if (tasks[i]->d_name[0] && end && !*end && tid > 0) {
                snprintf(path, sizeof(path), "/proc/%ld/task/%ld", (long)process->pid, tid);
                snprintf(key, sizeof(key), "%s/task/%ld", tag, tid);
                descriptors(output, key, path);
                const char *const aliases[] = {"cwd", "root"};
                for (size_t j = 0; j < 2; ++j) {
                    char alias[PATH_MAX + 16], alias_tag[512];
                    snprintf(alias, sizeof(alias), "%s/%s", path, aliases[j]);
                    snprintf(alias_tag, sizeof(alias_tag), "%s/%s", key, aliases[j]);
                    stat_path(output, alias_tag, alias);
                    link_path(output, alias_tag, alias);
                }
            }
            free(tasks[i]);
        }
        free(tasks);
    }
    fprintf(output, "PROCESS_COMPLETE %s\n", tag);
    const char *role = NULL;
    if (guest_name[0] && !strcmp(comm, guest_name)) role = "guest";
    else if (carrier_name[0] && !strcmp(comm, carrier_name) && innermost_pid >= 2) role = "carrier";
    if (role) {
        fprintf(output, "ROLE %s kind=%s comm=", tag, role);
        hex(output, (unsigned char *)comm, strlen(comm));
        fputc('\n', output);
        snprintf(observed_role, sizeof(observed_role), "%s", tag);
    }
}

static void observe_tree(FILE *output, pid_t root, int force) {
    /* Start at the one child we forked. Discover children through every known
     * task, never by enumerating/filtering the caller's process table. Retain
     * known identities if their parent retires or they are reparented. */
    if (!owned_count) {
        struct process process;
        if (process_identity(root, &process) == 0) {
            owned[owned_count++] = (struct owned_process){process, 0};
            fprintf(output, "LIFECYCLE created pid=%ld parent=%ld start=%llu\n",
                    (long)process.pid, (long)process.parent, process.start);
        }
    }
    for (size_t index = 0; index < owned_count; ++index) {
        if (owned[index].retired) continue;
        struct process process;
        if (process_identity(owned[index].identity.pid, &process) < 0 ||
            process.start != owned[index].identity.start) {
            owned[index].retired = 1;
            fprintf(output, "LIFECYCLE retired pid=%ld start=%llu\n",
                    (long)owned[index].identity.pid, owned[index].identity.start);
            continue;
        }
        observe(output, &process, force);
        if (children_unavailable) continue;
        char path[128];
        snprintf(path, sizeof(path), "/proc/%ld/task", (long)process.pid);
        struct dirent **tasks = NULL;
        int count = scandir(path, &tasks, NULL, alphasort);
        if (count < 0) {
            fprintf(output, "CHILDREN_ERROR process pid=%ld errno=%d\n", (long)process.pid, errno);
            continue;
        }
        for (int i = 0; i < count; ++i) {
            char *end = NULL;
            long tid = strtol(tasks[i]->d_name, &end, 10);
            if (tasks[i]->d_name[0] && end && !*end && tid > 0) {
                snprintf(path, sizeof(path), "/proc/%ld/task/%ld/children", (long)process.pid, tid);
                int fd = open(path, O_RDONLY | O_CLOEXEC);
                if (fd < 0) {
                    fprintf(output, "CHILDREN_ERROR task pid=%ld tid=%ld errno=%d\n", (long)process.pid, tid, errno);
                } else {
                    unsigned char bytes[16384];
                    ssize_t size = read(fd, bytes, sizeof(bytes) - 1);
                    close(fd);
                    if (size < 0 || size == (ssize_t)sizeof(bytes) - 1) {
                        error(output, "children-incomplete", size < 0 ? errno : EOVERFLOW);
                    } else {
                        fprintf(output, "CHILDREN task pid=%ld tid=%ld complete=1 bytes=", (long)process.pid, tid);
                        hex(output, bytes, (size_t)size);
                        fputc('\n', output);
                        bytes[size] = 0;
                        char *next = (char *)bytes;
                        while (*next) {
                            while (*next == ' ' || *next == '\n') ++next;
                            if (!*next) break;
                            long pid = strtol(next, &end, 10);
                            if (end == next || pid <= 0) { error(output, "children-malformed", EINVAL); break; }
                            next = end;
                            struct process child;
                            if (process_identity((pid_t)pid, &child) < 0) {
                                fprintf(output, "CHILDREN_EXIT task pid=%ld child=%ld\n", (long)process.pid, pid);
                                continue;
                            }
                            int seen = 0;
                            for (size_t j = 0; j < owned_count; ++j)
                                if (owned[j].identity.pid == child.pid && owned[j].identity.start == child.start) seen = 1;
                            if (seen) continue;
                            if (owned_count == DESCENDANT_LIMIT) { error(output, "descendant-cap", EOVERFLOW); break; }
                            owned[owned_count++] = (struct owned_process){child, 0};
                            fprintf(output, "LIFECYCLE created pid=%ld parent=%ld start=%llu\n",
                                    (long)child.pid, (long)child.parent, child.start);
                        }
                    }
                }
            }
            free(tasks[i]);
        }
        free(tasks);
    }
    fflush(output);
}

static int duplicate(int source, int destination) {
    if (dup2(source, destination) < 0) return -1;
    if (source != destination) close(source);
    return destination;
}

static char *number(unsigned long long value) {
    char *bytes = malloc(32);
    if (bytes) snprintf(bytes, 32, "%llu", value);
    return bytes;
}

static void append_object(char **arguments, size_t *count, const char *option, int fd) {
    struct stat st;
    if (fstat(fd, &st) < 0) { perror("sentinel stat"); exit(2); }
    arguments[(*count)++] = (char *)option;
    arguments[(*count)++] = number((unsigned long long)st.st_dev);
    arguments[(*count)++] = number((unsigned long long)st.st_ino);
}

int main(int argc, char **argv) {
    const char *report = NULL, *sentinel_directory = NULL;
    int append = 1, first = 1, eager_input = 0;
    while (first < argc && strcmp(argv[first], "--")) {
        if (first + 1 < argc && !strcmp(argv[first], "--observe")) report = argv[++first];
        else if (first + 1 < argc && !strcmp(argv[first], "--sentinel-directory")) sentinel_directory = argv[++first];
        else if (!strcmp(argv[first], "--no-guest-args")) append = 0;
        else if (!strcmp(argv[first], "--eager-input")) eager_input = 1;
        else if (first + 1 < argc && !strcmp(argv[first], "--guest-name")) snprintf(guest_name, sizeof(guest_name), "%.15s", argv[++first]);
        else if (first + 1 < argc && !strcmp(argv[first], "--carrier-name")) snprintf(carrier_name, sizeof(carrier_name), "%.15s", argv[++first]);
        else { fputs("unknown native launcher option\n", stderr); return 2; }
        ++first;
    }
    ++first;
    if (!report || !sentinel_directory || first >= argc) {
        fputs("usage: native-launcher --observe FILE --sentinel-directory DIR [--no-guest-args] -- CLI ARGS\n", stderr);
        return 2;
    }
    FILE *output = fopen(report, "we");
    if (!output) { perror("observer report"); return 2; }
    char **arguments = calloc((size_t)argc + 64, sizeof(*arguments));
    if (!arguments) return 2;
    size_t argument_count = 0;
    for (int i = first; i < argc; ++i) arguments[argument_count++] = argv[i];
    int directory = duplicate(open(sentinel_directory, O_RDONLY | O_DIRECTORY | O_CLOEXEC), 42);
    if (directory < 0) { perror("sentinel directory"); return 2; }
    int marker = duplicate(openat(directory, "descriptor-marker", O_RDONLY | O_CLOEXEC), 44);
    if (marker < 0) { perror("sentinel file"); return 2; }
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    char socket_name[80];
    snprintf(socket_name, sizeof(socket_name), "backend-isolation-sentinel-%ld", (long)getpid());
    size_t socket_length = strlen(socket_name);
    memcpy(address.sun_path + 1, socket_name, socket_length);
    int host_socket = duplicate(socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0), 43);
    if (host_socket < 0 || bind(host_socket, (struct sockaddr *)&address,
            (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + socket_length)) < 0 ||
        listen(host_socket, 8) < 0) { perror("sentinel socket"); return 2; }
    int ipc = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
    void *attachment = ipc < 0 ? (void *)-1 : shmat(ipc, NULL, 0);
    if (attachment == (void *)-1 || shmctl(ipc, IPC_RMID, NULL) < 0) {
        perror("sentinel IPC"); return 2;
    }
    /* Marked for removal while this owned attachment keeps it observable;
     * kernel cleanup also applies if the outer resource runner kills us. */
    fputs("NATIVE_SCHEMA backend-isolation-physical-v1\n", output);
    struct process baseline;
    if (process_identity(getpid(), &baseline) < 0) return 2;
    fprintf(output, "HOST_PID sentinel pid=%ld start=%llu\n", (long)getpid(), baseline.start);
    file(output, "host/kernel_hostname", "/proc/sys/kernel/hostname");
    file(output, "host/mountinfo", "/proc/self/mountinfo");
    directory_view(output, "host/interfaces", "/sys/class/net");
    char children_path[128];
    snprintf(children_path, sizeof(children_path), "/proc/self/task/%ld/children", (long)getpid());
    int children = open(children_path, O_RDONLY | O_CLOEXEC);
    if (children < 0) {
        children_unavailable = 1;
        error(output, "descendant-children-api", errno);
    } else close(children);
    for (size_t i = 0; i < 7; ++i) {
        char path[128];
        snprintf(path, sizeof(path), "/proc/self/ns/%s", namespace_names[i]);
        struct stat st;
        if (stat(path, &st) < 0) { perror("host namespace"); return 2; }
        fprintf(output, "HOST_NS %s dev=%llu ino=%llu\n", namespace_names[i],
                (unsigned long long)st.st_dev, (unsigned long long)st.st_ino);
        if (append) {
            arguments[argument_count++] = "--host-ns";
            arguments[argument_count++] = (char *)namespace_names[i];
            arguments[argument_count++] = number((unsigned long long)st.st_dev);
            arguments[argument_count++] = number((unsigned long long)st.st_ino);
        }
    }
    struct stat tmp;
    if (stat("/tmp", &tmp) < 0) { perror("host tmp"); return 2; }
    fprintf(output, "HOST_TMP tmp dev=%llu ino=%llu\n", (unsigned long long)tmp.st_dev, (unsigned long long)tmp.st_ino);
    if (append) {
        arguments[argument_count++] = "--host-tmp";
        arguments[argument_count++] = number((unsigned long long)tmp.st_dev);
        arguments[argument_count++] = number((unsigned long long)tmp.st_ino);
        append_object(arguments, &argument_count, "--host-directory", directory);
        append_object(arguments, &argument_count, "--host-file", marker);
        append_object(arguments, &argument_count, "--host-socket-object", host_socket);
        arguments[argument_count++] = "--host-pid";
        arguments[argument_count++] = number((unsigned long long)getpid());
        arguments[argument_count++] = "--host-ipc";
        arguments[argument_count++] = number((unsigned long long)ipc);
        arguments[argument_count++] = "--host-socket";
        arguments[argument_count++] = socket_name;
    }
    int input[2], guest_output[2];
    if (pipe2(input, O_CLOEXEC) < 0 || pipe2(guest_output, O_CLOEXEC | O_NONBLOCK) < 0) return 2;
    if (eager_input) {
        unsigned char acknowledgment = 'A';
        if (write(input[1], &acknowledgment, 1) != 1) return 2;
        close(input[1]);
        input[1] = -1;
        fputs("INPUT stdin policy=finite bytes=41 eof=1\n", output);
    }
    fflush(output); /* Do not let the fork copy duplicate the host receipt. */
    pid_t child = fork();
    if (child < 0) { perror("fork CLI"); return 2; }
    if (!child) {
        fclose(output);
        close(input[1]); close(guest_output[0]);
        if (dup2(input[0], STDIN_FILENO) < 0 || dup2(guest_output[1], STDOUT_FILENO) < 0) _exit(2);
        close(input[0]); close(guest_output[1]);
        int flags = fcntl(STDOUT_FILENO, F_GETFL);
        if (flags < 0 || fcntl(STDOUT_FILENO, F_SETFL, flags & ~O_NONBLOCK) < 0) _exit(2);
        execv(arguments[0], arguments);
        perror("exec CLI");
        _exit(2);
    }
    close(input[0]); close(guest_output[1]);
    signal(SIGPIPE, SIG_IGN); /* Parent only; do not change the guest disposition. */
    int status = 0, status_observed = 0, finished = 0, output_eof = 0;
    unsigned checkpoints = 0, acknowledged = eager_input;
    const char ready[] = "BARRIER_READY stdin\n";
    size_t matched = 0;
    unsigned long long last_scan = 0;
    while (!finished || !output_eof) {
        struct timespec time;
        if (clock_gettime(CLOCK_MONOTONIC, &time) < 0) { error(output, "observer-clock", errno); break; }
        unsigned long long now = (unsigned long long)time.tv_sec * 1000000000 + (unsigned long long)time.tv_nsec;
        if (!finished && now - last_scan >= 2000000) {
            observe_tree(output, child, 0);
            last_scan = now;
        }
        if (!acknowledged && observed_role[0]) {
            observe_tree(output, child, 1);
            unsigned char acknowledgment = 'A';
            ssize_t count = write(input[1], &acknowledgment, 1);
            fprintf(output, "ACK %s rc=%ld errno=%d predicate=complete-native-role\n", observed_role,
                    (long)count, count < 0 ? errno : 0);
            if (count == 1) acknowledged = 1;
            else error(output, "stdin-acknowledgment", errno);
        }
        char bytes[65536];
        ssize_t size = read(guest_output[0], bytes, sizeof(bytes));
        if (size == 0) output_eof = 1;
        if (size > 0) {
            size_t sent = 0;
            while (sent < (size_t)size) {
                ssize_t count = write(STDOUT_FILENO, bytes + sent, (size_t)size - sent);
                if (count <= 0) { error(output, "stdout-forward", errno); break; }
                sent += (size_t)count;
            }
            for (ssize_t i = 0; i < size; ++i) {
                matched = bytes[i] == ready[matched] ? matched + 1 : (bytes[i] == ready[0] ? 1 : 0);
                if (matched == sizeof(ready) - 1) {
                    ++checkpoints;
                    fprintf(output, "CHECKPOINT stdin sequence=%u\n", checkpoints);
                    matched = 0;
                }
            }
        }
        if (!finished) {
            pid_t result = waitpid(child, &status, WNOHANG);
            if (result == child) { finished = 1; status_observed = 1; }
            else if (result < 0) { error(output, "waitpid", errno); finished = 1; }
        }
        if (!finished || !output_eof) {
            struct pollfd descriptor = { .fd = guest_output[0], .events = POLLIN };
            poll(&descriptor, 1, 2);
        }
    }
    close(input[1]); close(guest_output[0]);
    observe_tree(output, child, 0);
    shmdt(attachment);
    size_t live = 0;
    for (size_t i = 0; i < owned_count; ++i) if (!owned[i].retired) ++live;
    if (status_observed) {
        fprintf(output, "CLI_EXIT child exited=%d code=%d signal=%d\n", WIFEXITED(status),
                WIFEXITED(status) ? WEXITSTATUS(status) : -1, WIFSIGNALED(status) ? WTERMSIG(status) : 0);
    }
    fprintf(output, "NATIVE_COMPLETE backend-isolation-physical-v1 snapshots=%zu identities=%zu checkpoints=%u errors=%u live=%zu\n",
            snapshot_count, identity_count, checkpoints, observer_errors, live);
    int report_error = ferror(output);
    fclose(output);
    if (report_error || observer_errors || !status_observed) return 2;
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return 2;
}
