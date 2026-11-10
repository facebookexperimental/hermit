/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// The CPUs a program can see, and how CPU affinity behaves.
//
// `virtual_cpus counts` prints, one per line, how many CPUs each source a
// program may size a thread pool from reports: sysconf, glibc's get_nprocs,
// sched_getaffinity, the CPU lists and directories in /sys, /proc/cpuinfo,
// /proc/stat, /proc/self/status, node 0, the distinct APIC IDs CPUID reports
// on each CPU of the affinity mask, the CPUs on which getcpu, sched_getcpu and
// /proc/thread-self/stat name the CPU the thread is pinned to, and the
// distinct CPUs of one unpinned thread per CPU. On a machine of N CPUs every
// line says N.
//
// `virtual_cpus affinity` checks sched_setaffinity and what follows from it on
// the first three CPUs of the caller's mask: a set mask reads back; Linux's
// errors for an empty mask, a mask of no existing CPU, a zero length, an
// unreadable mask, no such task and a short getaffinity length; a CPU past
// the last one dropped from a mask; getcpu and /proc/thread-self/status on
// the pinned CPU; inheritance by a thread and a child process; a thread
// created with an affinity attribute; and a mask one thread sets on another.
// Each line says what was checked and whether it held, so the output is the
// same natively and on any machine with three CPUs.
#define _GNU_SOURCE
#include <cpuid.h>
#include <dirent.h>
#include <errno.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/wait.h>
#include <unistd.h>

// Room for 8192 CPUs, more than any kernel's mask: sched_getaffinity copies
// at most its own mask's size.
#define MAX_CPUS 8192
#define SET_SIZE CPU_ALLOC_SIZE(MAX_CPUS)

static cpu_set_t *new_set(void) {
    cpu_set_t *set = CPU_ALLOC(MAX_CPUS);
    if (set == NULL) {
        perror("CPU_ALLOC");
        exit(2);
    }
    CPU_ZERO_S(SET_SIZE, set);
    return set;
}

static long raw_getaffinity(pid_t pid, size_t size, cpu_set_t *set) {
    return syscall(SYS_sched_getaffinity, pid, size, set);
}

static long raw_setaffinity(pid_t pid, size_t size, const cpu_set_t *set) {
    return syscall(SYS_sched_setaffinity, pid, size, set);
}

// The calling thread's mask.
static cpu_set_t *own_mask(void) {
    cpu_set_t *set = new_set();
    if (raw_getaffinity(0, SET_SIZE, set) < 0) {
        perror("sched_getaffinity");
        exit(2);
    }
    return set;
}

static void pin(const cpu_set_t *set) {
    if (raw_setaffinity(0, SET_SIZE, set) != 0) {
        perror("sched_setaffinity");
        exit(2);
    }
}

// The number of CPUs in a kernel CPU list such as "0-3,8".
static int list_count(const char *list) {
    int count = 0;
    const char *at = list;
    while (*at != '\0' && *at != '\n') {
        char *end;
        long first = strtol(at, &end, 10);
        long last = first;
        if (*end == '-') {
            last = strtol(end + 1, &end, 10);
        }
        count += (int)(last - first + 1);
        at = *end == ',' ? end + 1 : end;
    }
    return count;
}

// The first line of `path`, without its newline, in `line`.
static int read_line(const char *path, char *line, size_t size) {
    FILE *file = fopen(path, "r");
    if (file == NULL || fgets(line, (int)size, file) == NULL) {
        if (file != NULL) {
            fclose(file);
        }
        return -1;
    }
    fclose(file);
    line[strcspn(line, "\n")] = '\0';
    return 0;
}

static int file_list_count(const char *path) {
    char line[4096];
    return read_line(path, line, sizeof(line)) == 0 ? list_count(line) : -1;
}

// The lines of `path` that start with `prefix`.
static int count_lines(const char *path, const char *prefix, int need_digit) {
    FILE *file = fopen(path, "r");
    if (file == NULL) {
        return -1;
    }
    char line[8192];
    int count = 0;
    size_t length = strlen(prefix);
    while (fgets(line, sizeof(line), file) != NULL) {
        if (strncmp(line, prefix, length) == 0 &&
            (!need_digit || (line[length] >= '0' && line[length] <= '9'))) {
            count++;
        }
    }
    fclose(file);
    return count;
}

// The value of the line of `path` that starts with `prefix`, in `value`.
static int line_value(const char *path, const char *prefix, char *value, size_t size) {
    FILE *file = fopen(path, "r");
    if (file == NULL) {
        return -1;
    }
    char line[8192];
    size_t length = strlen(prefix);
    int found = -1;
    while (fgets(line, sizeof(line), file) != NULL) {
        if (strncmp(line, prefix, length) == 0) {
            const char *start = line + length;
            start += strspn(start, " \t");
            snprintf(value, size, "%s", start);
            value[strcspn(value, "\n")] = '\0';
            found = 0;
            break;
        }
    }
    fclose(file);
    return found;
}

static int cpu_directories(void) {
    DIR *directory = opendir("/sys/devices/system/cpu");
    if (directory == NULL) {
        return -1;
    }
    int count = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        const char *number = entry->d_name + 3;
        if (strncmp(entry->d_name, "cpu", 3) == 0 && *number != '\0' &&
            strspn(number, "0123456789") == strlen(number)) {
            count++;
        }
    }
    closedir(directory);
    return count;
}

static unsigned current_cpu(void) {
    unsigned cpu = UINT_MAX;
    if (syscall(SYS_getcpu, &cpu, NULL, NULL) != 0) {
        perror("getcpu");
        exit(2);
    }
    return cpu;
}

// The CPU /proc/thread-self/stat names in field 39, `processor`, the CPU the
// thread last ran on; -1 where the file cannot be read.
static long stat_cpu(void) {
    char line[4096];
    FILE *file = fopen("/proc/thread-self/stat", "r");
    if (file == NULL) {
        return -1;
    }
    size_t length = fread(line, 1, sizeof(line) - 1, file);
    fclose(file);
    line[length] = '\0';
    char *field = strrchr(line, ')');
    if (field == NULL) {
        return -1;
    }
    // Field 3 follows the name; field 39 is 36 fields later.
    for (int skipped = 0; skipped < 37 && field != NULL; skipped++) {
        field = strchr(field + 1, ' ');
    }
    return field == NULL ? -1 : strtol(field + 1, NULL, 10);
}

static int contains(const unsigned *values, int count, unsigned value) {
    for (int i = 0; i < count; i++) {
        if (values[i] == value) {
            return 1;
        }
    }
    return 0;
}

static void *report_cpu(void *slot) {
    *(unsigned *)slot = current_cpu();
    return NULL;
}

// One thread per CPU, each unpinned, each reporting its CPU: how many
// distinct CPUs they report.
static int distinct_thread_cpus(int threads) {
    pthread_t *handles = calloc((size_t)threads, sizeof(pthread_t));
    unsigned *cpus = calloc((size_t)threads, sizeof(unsigned));
    unsigned *distinct = calloc((size_t)threads, sizeof(unsigned));
    if (handles == NULL || cpus == NULL || distinct == NULL) {
        exit(2);
    }
    for (int i = 0; i < threads; i++) {
        if (pthread_create(&handles[i], NULL, report_cpu, &cpus[i]) != 0) {
            exit(2);
        }
    }
    int count = 0;
    for (int i = 0; i < threads; i++) {
        pthread_join(handles[i], NULL);
        if (!contains(distinct, count, cpus[i])) {
            distinct[count++] = cpus[i];
        }
    }
    free(handles);
    free(cpus);
    free(distinct);
    return count;
}

static int counts(void) {
    printf("sysconf(_SC_NPROCESSORS_ONLN)=%ld\n", sysconf(_SC_NPROCESSORS_ONLN));
    printf("sysconf(_SC_NPROCESSORS_CONF)=%ld\n", sysconf(_SC_NPROCESSORS_CONF));
    printf("get_nprocs=%d\n", get_nprocs());
    printf("get_nprocs_conf=%d\n", get_nprocs_conf());
    cpu_set_t *original = own_mask();
    printf("sched_getaffinity=%d\n", CPU_COUNT_S(SET_SIZE, original));
    printf("/sys/devices/system/cpu/online=%d\n",
           file_list_count("/sys/devices/system/cpu/online"));
    printf("/sys/devices/system/cpu/possible=%d\n",
           file_list_count("/sys/devices/system/cpu/possible"));
    printf("/sys/devices/system/cpu/present=%d\n",
           file_list_count("/sys/devices/system/cpu/present"));
    printf("/sys/devices/system/cpu/cpuN=%d\n", cpu_directories());
    printf("/sys/devices/system/node/node0/cpulist=%d\n",
           file_list_count("/sys/devices/system/node/node0/cpulist"));
    printf("/proc/cpuinfo processor=%d\n", count_lines("/proc/cpuinfo", "processor", 0));
    printf("/proc/stat cpuN=%d\n", count_lines("/proc/stat", "cpu", 1));
    char allowed[4096];
    printf("/proc/self/status Cpus_allowed_list=%d\n",
           line_value("/proc/self/status", "Cpus_allowed_list:", allowed, sizeof(allowed)) == 0
               ? list_count(allowed)
               : -1);

    // On each CPU of the mask in turn: the APIC IDs CPUID reports there, and
    // whether getcpu, sched_getcpu and /proc/thread-self/stat name it.
    unsigned max_leaf = __get_cpuid_max(0, NULL);
    int cpus = CPU_COUNT_S(SET_SIZE, original);
    unsigned *apic = calloc((size_t)cpus, sizeof(unsigned));
    unsigned *x2apic = calloc((size_t)cpus, sizeof(unsigned));
    int apic_ids = 0, x2apic_ids = 0, named = 0;
    cpu_set_t *one = new_set();
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        if (!CPU_ISSET_S(cpu, SET_SIZE, original)) {
            continue;
        }
        CPU_ZERO_S(SET_SIZE, one);
        CPU_SET_S(cpu, SET_SIZE, one);
        pin(one);
        unsigned eax, ebx, ecx, edx;
        __cpuid(1, eax, ebx, ecx, edx);
        if (!contains(apic, apic_ids, ebx >> 24)) {
            apic[apic_ids++] = ebx >> 24;
        }
        if (max_leaf >= 0xb) {
            __cpuid_count(0xb, 0, eax, ebx, ecx, edx);
            if (!contains(x2apic, x2apic_ids, edx)) {
                x2apic[x2apic_ids++] = edx;
            }
        }
        named += current_cpu() == (unsigned)cpu && sched_getcpu() == cpu && stat_cpu() == cpu;
    }
    pin(original);
    printf("CPUID leaf 1 APIC IDs=%d\n", apic_ids);
    printf("CPUID leaf 0xb x2APIC IDs=%d\n", max_leaf >= 0xb ? x2apic_ids : cpus);
    printf("getcpu, sched_getcpu and stat on each pinned CPU=%d\n", named);
    printf("distinct CPUs of one thread per CPU=%d\n", distinct_thread_cpus(cpus));
    free(apic);
    free(x2apic);
    CPU_FREE(one);
    CPU_FREE(original);
    return 0;
}

static const char *outcome(long rc, int error) {
    static char text[64];
    if (rc >= 0) {
        snprintf(text, sizeof(text), "ok");
    } else {
        snprintf(text, sizeof(text), "%s", strerrorname_np(error));
    }
    return text;
}

static int same(const cpu_set_t *a, const cpu_set_t *b) {
    return CPU_EQUAL_S(SET_SIZE, a, b);
}

static const char *held(int holds) {
    return holds ? "yes" : "NO";
}

// The CPUs of a set as a kernel CPU list, "2,5".
static void list_of(const cpu_set_t *set, char *list, size_t size) {
    list[0] = '\0';
    for (int cpu = 0; cpu < MAX_CPUS; cpu++) {
        if (CPU_ISSET_S(cpu, SET_SIZE, set)) {
            size_t length = strlen(list);
            snprintf(list + length, size - length, "%s%d", length ? "," : "", cpu);
        }
    }
}

struct worker {
    // The thread waits for its mask to be set, reports, and stays alive
    // until the setter has read the mask back.
    pthread_barrier_t set, checked, read;
    const cpu_set_t *expected;
    int saw_expected, on_expected;
};

static void *pinned_worker(void *argument) {
    struct worker *worker = argument;
    pthread_barrier_wait(&worker->set);
    cpu_set_t *mask = own_mask();
    worker->saw_expected = same(mask, worker->expected);
    worker->on_expected = CPU_ISSET_S(current_cpu(), SET_SIZE, worker->expected);
    CPU_FREE(mask);
    pthread_barrier_wait(&worker->checked);
    pthread_barrier_wait(&worker->read);
    return NULL;
}

static void *inherited_worker(void *argument) {
    struct worker *worker = argument;
    cpu_set_t *mask = own_mask();
    worker->saw_expected = same(mask, worker->expected);
    worker->on_expected = CPU_ISSET_S(current_cpu(), SET_SIZE, worker->expected);
    CPU_FREE(mask);
    return NULL;
}

static void *attribute_worker(void *argument) {
    return inherited_worker(argument);
}

static int affinity(void) {
    cpu_set_t *original = own_mask();
    int cpu[3], found = 0;
    for (int i = 0; i < MAX_CPUS && found < 3; i++) {
        if (CPU_ISSET_S(i, SET_SIZE, original)) {
            cpu[found++] = i;
        }
    }
    if (found < 3) {
        printf("needs three CPUs, has %d\n", found);
        return 2;
    }
    cpu_set_t *want = new_set(), *got = new_set();
    long rc;
    int error;

    CPU_SET_S(cpu[0], SET_SIZE, want);
    CPU_SET_S(cpu[2], SET_SIZE, want);
    rc = raw_setaffinity(0, SET_SIZE, want);
    error = errno;
    raw_getaffinity(0, SET_SIZE, got);
    printf("first and third CPUs: %s, reads back: %s\n", outcome(rc, error),
           held(same(got, want)));

    cpu_set_t *before = own_mask(), *empty = new_set();
    rc = raw_setaffinity(0, SET_SIZE, empty);
    error = errno;
    raw_getaffinity(0, SET_SIZE, got);
    printf("empty mask: %s, mask unchanged: %s\n", outcome(rc, error), held(same(got, before)));

    cpu_set_t *beyond = new_set();
    CPU_SET_S(MAX_CPUS - 1, SET_SIZE, beyond);
    rc = raw_setaffinity(0, SET_SIZE, beyond);
    error = errno;
    printf("only a CPU the machine lacks: %s\n", outcome(rc, error));

    CPU_SET_S(cpu[0], SET_SIZE, beyond);
    CPU_ZERO_S(SET_SIZE, want);
    CPU_SET_S(cpu[0], SET_SIZE, want);
    rc = raw_setaffinity(0, SET_SIZE, beyond);
    error = errno;
    raw_getaffinity(0, SET_SIZE, got);
    printf("first CPU and one the machine lacks: %s, reads back the first: %s\n",
           outcome(rc, error), held(same(got, want)));

    // The CPU numbered as many as sysconf counts is past the last one; it is
    // dropped from a mask, as one past the machine's mask size is.
    long past = sysconf(_SC_NPROCESSORS_CONF);
    CPU_ZERO_S(SET_SIZE, beyond);
    CPU_SET_S(cpu[0], SET_SIZE, beyond);
    CPU_SET_S(past, SET_SIZE, beyond);
    rc = raw_setaffinity(0, SET_SIZE, beyond);
    error = errno;
    raw_getaffinity(0, SET_SIZE, got);
    printf("first CPU and the one past the last: %s, reads back the first: %s\n",
           outcome(rc, error), held(same(got, want)));

    rc = raw_setaffinity(0, 0, original);
    error = errno;
    printf("zero length: %s\n", outcome(rc, error));
    rc = raw_setaffinity(0, SET_SIZE, NULL);
    error = errno;
    printf("unreadable mask: %s\n", outcome(rc, error));
    rc = raw_setaffinity(INT_MAX, SET_SIZE, original);
    error = errno;
    printf("set, no such task: %s\n", outcome(rc, error));
    rc = raw_setaffinity(-1, SET_SIZE, original);
    error = errno;
    printf("set, negative pid: %s\n", outcome(rc, error));
    rc = raw_setaffinity(INT_MAX, SET_SIZE, empty);
    error = errno;
    printf("set an empty mask, no such task: %s\n", outcome(rc, error));
    rc = raw_getaffinity(INT_MAX, SET_SIZE, got);
    error = errno;
    printf("get, no such task: %s\n", outcome(rc, error));
    rc = raw_getaffinity(0, 4, got);
    error = errno;
    printf("get, length not a multiple of long: %s\n", outcome(rc, error));
    rc = raw_getaffinity(0, SET_SIZE, NULL);
    error = errno;
    printf("get, unwritable mask: %s\n", outcome(rc, error));

    // Pinned to the second CPU.
    CPU_ZERO_S(SET_SIZE, want);
    CPU_SET_S(cpu[1], SET_SIZE, want);
    pin(want);
    char expected[64], status[4096];
    list_of(want, expected, sizeof(expected));
    int status_read =
        line_value("/proc/thread-self/status", "Cpus_allowed_list:", status, sizeof(status));
    printf("pinned to the second CPU: getcpu: %s, sched_getcpu: %s, status: %s\n",
           held(current_cpu() == (unsigned)cpu[1]), held(sched_getcpu() == cpu[1]),
           held(status_read == 0 && strcmp(status, expected) == 0));

    struct worker inherited = {.expected = want};
    pthread_t thread;
    pthread_create(&thread, NULL, inherited_worker, &inherited);
    pthread_join(thread, NULL);
    printf("a new thread inherits the mask: %s, runs on it: %s\n", held(inherited.saw_expected),
           held(inherited.on_expected));

    pid_t child = fork();
    if (child == 0) {
        cpu_set_t *mask = own_mask();
        _exit(same(mask, want) && current_cpu() == (unsigned)cpu[1] ? 0 : 1);
    }
    int child_status = 0;
    waitpid(child, &child_status, 0);
    printf("a child process inherits the mask: %s\n",
           held(WIFEXITED(child_status) && WEXITSTATUS(child_status) == 0));

    // A thread created with an affinity attribute: glibc sets its mask from
    // the creating thread before the new one runs.
    pthread_attr_t attribute;
    pthread_attr_init(&attribute);
    cpu_set_t *first = new_set();
    CPU_SET_S(cpu[0], SET_SIZE, first);
    pthread_attr_setaffinity_np(&attribute, SET_SIZE, first);
    struct worker attributed = {.expected = first};
    int create_rc = pthread_create(&thread, &attribute, attribute_worker, &attributed);
    if (create_rc == 0) {
        pthread_join(thread, NULL);
    }
    pthread_attr_destroy(&attribute);
    printf("a thread created with an affinity attribute: %s, has the mask: %s, runs on it: %s\n",
           create_rc == 0 ? "ok" : strerrorname_np(create_rc), held(attributed.saw_expected),
           held(attributed.on_expected));

    // One thread sets another's mask, as pthread_setaffinity_np does.
    cpu_set_t *third = new_set();
    CPU_SET_S(cpu[2], SET_SIZE, third);
    struct worker worker = {.expected = third};
    pthread_barrier_init(&worker.set, NULL, 2);
    pthread_barrier_init(&worker.checked, NULL, 2);
    pthread_barrier_init(&worker.read, NULL, 2);
    pthread_create(&thread, NULL, pinned_worker, &worker);
    int set_rc = pthread_setaffinity_np(thread, SET_SIZE, third);
    pthread_barrier_wait(&worker.set);
    pthread_barrier_wait(&worker.checked);
    int get_rc = pthread_getaffinity_np(thread, SET_SIZE, got);
    pthread_barrier_wait(&worker.read);
    pthread_join(thread, NULL);
    printf("another thread's mask: set %s, it sees it: %s, runs on it: %s, read back: %s\n",
           set_rc == 0 ? "ok" : strerrorname_np(set_rc), held(worker.saw_expected),
           held(worker.on_expected), held(get_rc == 0 && same(got, third)));
    pthread_barrier_destroy(&worker.set);
    pthread_barrier_destroy(&worker.checked);
    pthread_barrier_destroy(&worker.read);
    raw_getaffinity(0, SET_SIZE, got);
    printf("the caller's own mask is unchanged: %s\n", held(same(got, want)));

    pin(original);
    CPU_FREE(original);
    CPU_FREE(want);
    CPU_FREE(got);
    CPU_FREE(before);
    CPU_FREE(empty);
    CPU_FREE(beyond);
    CPU_FREE(third);
    CPU_FREE(first);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "counts") == 0) {
        return counts();
    }
    if (argc == 2 && strcmp(argv[1], "affinity") == 0) {
        return affinity();
    }
    fprintf(stderr, "usage: %s counts|affinity\n", argv[0]);
    return 2;
}
