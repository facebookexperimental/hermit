/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Every view of a task's CPU time agrees: the utime, stime, cutime and
// cstime of /proc/<pid>/stat and /proc/<pid>/task/<tid>/stat, the runtime in
// /proc/<pid>/schedstat, times(2), getrusage(2) and the CPU-time clocks.
// Each check reads one view between two reads of another, or while the task
// it reads sleeps, and prints only whether it held, so the output is the same
// natively and under Hermit:
//
// - the process's stat between two times() calls, field by field;
// - the thread's stat between two getrusage(RUSAGE_THREAD) calls;
// - the thread's schedstat between two reads of its CPU clock (reading the
//   clock brings the kernel's runtime up to date), and a schedstat read again
//   after a rewind;
// - a sleeping sibling's stat and schedstat against its CPU clock, and the
//   leader's /proc/self/schedstat, read by the sibling while the leader
//   sleeps, against the leader's CPU clock;
// - the process's stat after the sibling exits, which still counts it;
// - the reaped children's time in the process's and the thread's stat,
//   times() and getrusage(RUSAGE_CHILDREN).
//
// The burn loop's system calls give Hermit's runs system time too; natively
// they cost less than a clock tick, so the stime and cstime checks compare
// zeros there. Natively the kernel must show schedstat (CONFIG_SCHED_INFO);
// Hermit lists it at --proc=standard.
//
// `proc_cpu_times values` also prints the times each check read, which differ
// from run to run natively and are the same in every run under Hermit.
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/times.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define NANOS_PER_TICK 10000000ULL
#define BURN_ITERATIONS 50000000L
#define BURN_SYSCALLS 5000

static volatile unsigned long sink;

// Spend CPU: enough user time for several clock ticks natively and under
// Hermit, and system calls.
static void burn(void) {
    for (long i = 0; i < BURN_ITERATIONS; i++) {
        sink += (unsigned long)i ^ (sink >> 3);
    }
    // A system call Detcore intercepts and charges system time for.
    struct timespec now;
    for (int i = 0; i < BURN_SYSCALLS; i++) {
        syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &now);
    }
}

static int print_values;

// In `values` mode, print one value a check read.
static void value(const char *name, unsigned long long reading) {
    if (print_values) {
        printf("value %s %llu\n", name, reading);
    }
}

static const char *held(int condition) {
    return condition ? "yes" : "NO";
}

static int read_file(const char *path, char *buffer, size_t size) {
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t length = read(fd, buffer, size - 1);
    close(fd);
    if (length < 0) {
        return -1;
    }
    buffer[length] = '\0';
    return 0;
}

struct stat_times {
    unsigned long long utime, stime, cutime, cstime;
};

// Fields 14 to 17 of a stat file, in clock ticks.
static int stat_times(const char *path, struct stat_times *times) {
    char line[4096];
    if (read_file(path, line, sizeof(line)) != 0) {
        return -1;
    }
    char *field = strrchr(line, ')');
    if (field == NULL) {
        return -1;
    }
    // The name ends field 2; field 14 follows 12 more separators.
    for (int skipped = 0; skipped < 12 && field != NULL; skipped++) {
        field = strchr(field + 1, ' ');
    }
    if (field == NULL ||
        sscanf(field, " %llu %llu %llu %llu", &times->utime, &times->stime, &times->cutime,
               &times->cstime) != 4) {
        return -1;
    }
    return 0;
}

// The runtime a schedstat line shows, in nanoseconds.
static unsigned long long parse_runtime(const char *line) {
    unsigned long long runtime = 0;
    return sscanf(line, "%llu", &runtime) == 1 ? runtime : (unsigned long long)-1;
}

// The runtime a schedstat file shows, in nanoseconds.
static unsigned long long schedstat_runtime(const char *path) {
    char line[256];
    return read_file(path, line, sizeof(line)) == 0 ? parse_runtime(line)
                                                    : (unsigned long long)-1;
}

// The runtime the schedstat open as `fd` shows, read from the start.
static unsigned long long schedstat_runtime_at_start(int fd) {
    char line[256];
    ssize_t length = pread(fd, line, sizeof(line) - 1, 0);
    if (length <= 0) {
        return (unsigned long long)-1;
    }
    line[length] = '\0';
    return parse_runtime(line);
}

static unsigned long long clock_nanos(clockid_t clock) {
    struct timespec now;
    if (clock_gettime(clock, &now) != 0) {
        perror("clock_gettime");
        exit(2);
    }
    return (unsigned long long)now.tv_sec * 1000000000ULL + (unsigned long long)now.tv_nsec;
}

// A getrusage time in clock ticks, as Linux truncates both.
static unsigned long long timeval_ticks(struct timeval value) {
    return ((unsigned long long)value.tv_sec * 1000000ULL + (unsigned long long)value.tv_usec) /
           (NANOS_PER_TICK / 1000);
}

// Wait until the task `path` describes sleeps (stat's state `S`), so that
// its CPU time no longer moves.
static void wait_until_sleeping(const char *path) {
    for (int attempt = 0; attempt < 10000; attempt++) {
        char line[4096];
        char *state = read_file(path, line, sizeof(line)) == 0 ? strrchr(line, ')') : NULL;
        if (state != NULL && state[1] == ' ' && state[2] == 'S') {
            return;
        }
        usleep(100);
    }
}

static pthread_barrier_t worker_ran, worker_may_exit;
static pid_t worker_tid;
static pthread_t leader;
static int leader_schedstat_matches;

static void *worker(void *unused) {
    (void)unused;
    worker_tid = (pid_t)syscall(SYS_gettid);
    burn();
    // The leader sleeps in the barrier: /proc/self/schedstat describes it,
    // not this thread that opens it, and shows its CPU clock.
    clockid_t leader_clock;
    pthread_getcpuclockid(leader, &leader_clock);
    for (int attempt = 0; attempt < 1000 && !leader_schedstat_matches; attempt++) {
        unsigned long long before = clock_nanos(leader_clock);
        unsigned long long runtime = schedstat_runtime("/proc/self/schedstat");
        unsigned long long after = clock_nanos(leader_clock);
        leader_schedstat_matches = before == after && runtime == before;
        if (!leader_schedstat_matches) {
            usleep(100);
        }
    }
    pthread_barrier_wait(&worker_ran);
    pthread_barrier_wait(&worker_may_exit);
    return NULL;
}

int main(int argc, char **argv) {
    print_values = argc > 1 && strcmp(argv[1], "values") == 0;
    burn();

    // The process's stat between two times() calls.
    struct tms before, after;
    struct stat_times process = {0};
    times(&before);
    int process_read = stat_times("/proc/self/stat", &process);
    times(&after);
    value("process-stat-utime", process.utime);
    value("process-stat-stime", process.stime);
    printf("the process has used CPU: %s\n", held(after.tms_utime + after.tms_stime > 0));
    printf("process stat utime between times() calls: %s\n",
           held(process_read == 0 && (unsigned long long)before.tms_utime <= process.utime &&
                process.utime <= (unsigned long long)after.tms_utime));
    printf("process stat stime between times() calls: %s\n",
           held(process_read == 0 && (unsigned long long)before.tms_stime <= process.stime &&
                process.stime <= (unsigned long long)after.tms_stime));

    // The thread's stat between two getrusage(RUSAGE_THREAD) calls.
    struct rusage usage_before, usage_after;
    struct stat_times thread = {0};
    getrusage(RUSAGE_THREAD, &usage_before);
    int thread_read = stat_times("/proc/thread-self/stat", &thread);
    getrusage(RUSAGE_THREAD, &usage_after);
    value("thread-stat-utime", thread.utime);
    value("thread-stat-stime", thread.stime);
    printf("thread stat utime between getrusage(RUSAGE_THREAD) calls: %s\n",
           held(thread_read == 0 && timeval_ticks(usage_before.ru_utime) <= thread.utime &&
                thread.utime <= timeval_ticks(usage_after.ru_utime)));
    printf("thread stat stime between getrusage(RUSAGE_THREAD) calls: %s\n",
           held(thread_read == 0 && timeval_ticks(usage_before.ru_stime) <= thread.stime &&
                thread.stime <= timeval_ticks(usage_after.ru_stime)));

    // The thread's schedstat between two reads of its CPU clock; this thread
    // leads, so /proc/self/schedstat is its too.
    const char *own_schedstats[] = {"/proc/thread-self/schedstat", "/proc/self/schedstat"};
    for (int i = 0; i < 2; i++) {
        unsigned long long clock_before = clock_nanos(CLOCK_THREAD_CPUTIME_ID);
        unsigned long long runtime = schedstat_runtime(own_schedstats[i]);
        unsigned long long clock_after = clock_nanos(CLOCK_THREAD_CPUTIME_ID);
        value(own_schedstats[i], runtime);
        printf("%s runtime between thread CPU clock reads: %s\n", own_schedstats[i],
               held(clock_before <= runtime && runtime <= clock_after));
    }
    // A schedstat read again from the start shows the runtime then.
    int schedstat = open("/proc/thread-self/schedstat", O_RDONLY | O_CLOEXEC);
    unsigned long long first = schedstat_runtime_at_start(schedstat);
    burn();
    unsigned long long clock_before = clock_nanos(CLOCK_THREAD_CPUTIME_ID);
    unsigned long long again = schedstat_runtime_at_start(schedstat);
    unsigned long long clock_after = clock_nanos(CLOCK_THREAD_CPUTIME_ID);
    close(schedstat);
    printf("a schedstat read again from the start shows the runtime then: %s\n",
           held(first < again && clock_before <= again && again <= clock_after));

    // A sibling that has run and now sleeps: its stat and schedstat against
    // its CPU clock.
    pthread_barrier_init(&worker_ran, NULL, 2);
    pthread_barrier_init(&worker_may_exit, NULL, 2);
    leader = pthread_self();
    pthread_t handle;
    pthread_create(&handle, NULL, worker, NULL);
    pthread_barrier_wait(&worker_ran);
    char path[64];
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat", worker_tid);
    wait_until_sleeping(path);
    clockid_t worker_clock;
    pthread_getcpuclockid(handle, &worker_clock);
    char schedstat_path[64];
    snprintf(schedstat_path, sizeof(schedstat_path), "/proc/self/task/%d/schedstat", worker_tid);
    // Read the sibling's files between two equal readings of its clock: it
    // did not run in between.
    unsigned long long worker_time = 0, sibling_runtime = 0;
    struct stat_times sibling = {0};
    int sibling_read = -1;
    for (int attempt = 0; attempt < 1000; attempt++) {
        worker_time = clock_nanos(worker_clock);
        sibling_runtime = schedstat_runtime(schedstat_path);
        sibling_read = stat_times(path, &sibling);
        if (clock_nanos(worker_clock) == worker_time) {
            break;
        }
        usleep(100);
    }
    unsigned long long sibling_ticks = sibling.utime + sibling.stime;
    unsigned long long clock_ticks = worker_time / NANOS_PER_TICK;
    value("sibling-cpu-clock", worker_time);
    value("sibling-stat-time", sibling_ticks);
    printf("the sibling has used CPU: %s\n", held(worker_time > 0));
    printf("sibling stat time matches its CPU clock within a tick: %s\n",
           held(sibling_read == 0 && sibling_ticks <= clock_ticks && sibling_ticks + 1 >= clock_ticks));
    printf("sibling schedstat runtime equals its CPU clock: %s\n",
           held(sibling_runtime == worker_time));
    printf("the leader's schedstat, read by the sibling, equals the leader's CPU clock: %s\n",
           held(leader_schedstat_matches));
    pthread_barrier_wait(&worker_may_exit);
    pthread_join(handle, NULL);

    // The process's time still counts the exited sibling's
    // (thread_group_cputime): at least the leader's and the sibling's, less
    // a tick each truncation may take.
    struct stat_times leader_stat = {0}, joined = {0};
    int leader_read = stat_times("/proc/thread-self/stat", &leader_stat);
    int joined_read = stat_times("/proc/self/stat", &joined);
    printf("the process's stat counts the exited sibling: %s\n",
           held(leader_read == 0 && joined_read == 0 &&
                joined.utime + joined.stime + 1 >=
                    leader_stat.utime + leader_stat.stime + sibling_ticks));

    // A reaped child's time: stat's cutime and cstime, times() and
    // getrusage(RUSAGE_CHILDREN) agree.
    pid_t child = fork();
    if (child == 0) {
        burn();
        _exit(0);
    }
    waitpid(child, NULL, 0);
    struct tms reaped;
    struct rusage children;
    struct stat_times with_child = {0}, thread_with_child = {0};
    times(&reaped);
    getrusage(RUSAGE_CHILDREN, &children);
    int child_read = stat_times("/proc/self/stat", &with_child);
    int thread_child_read = stat_times("/proc/thread-self/stat", &thread_with_child);
    value("stat-cutime", with_child.cutime);
    value("stat-cstime", with_child.cstime);
    printf("the reaped child has used CPU: %s\n",
           held(reaped.tms_cutime + reaped.tms_cstime > 0));
    printf("stat cutime and cstime equal times(): %s\n",
           held(child_read == 0 && with_child.cutime == (unsigned long long)reaped.tms_cutime &&
                with_child.cstime == (unsigned long long)reaped.tms_cstime));
    printf("stat cutime and cstime equal getrusage(RUSAGE_CHILDREN): %s\n",
           held(child_read == 0 && with_child.cutime == timeval_ticks(children.ru_utime) &&
                with_child.cstime == timeval_ticks(children.ru_stime)));
    printf("the thread's stat shows the process's children's time: %s\n",
           held(thread_child_read == 0 &&
                thread_with_child.cutime == (unsigned long long)reaped.tms_cutime &&
                thread_with_child.cstime == (unsigned long long)reaped.tms_cstime));
    return 0;
}
