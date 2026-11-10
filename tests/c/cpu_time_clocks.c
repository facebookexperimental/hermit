/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Checks Linux's CPU-time clocks by their own rules and against the other
// CPU-time views of the same tasks, and prints one line per check, never a
// time, so the output is the same natively and under Hermit:
//
// - CLOCK_THREAD_CPUTIME_ID and CLOCK_PROCESS_CPUTIME_ID read CPU time, far
//   below the wall clock, and advance with the work a thread does;
// - a new thread's clock starts near zero, and a blocked thread's stands
//   still;
// - the clock ids pthread_getcpuclockid and clock_getcpuclockid build read the
//   same clocks, for the caller and for another thread;
// - the process clock covers every thread's, exited ones included, and an
//   exited thread's id names no clock any more, except a leader's: a leader
//   that exits before its other thread keeps reading its final time;
// - clock(), getrusage and times agree with CLOCK_PROCESS_CPUTIME_ID and
//   CLOCK_THREAD_CPUTIME_ID, to their own units;
// - a fork child's clocks start afresh, and an exec keeps the thread's and
//   the process's;
// - clock_nanosleep refuses the caller's own thread clock as Linux does, and
//   returns at once for a process-clock deadline that has passed. (A sleep
//   that would have to wait is not checked: Hermit refuses it, since only the
//   clock's task's CPU time may end it.)
//
// Before, Hermit answered every CPU clock with its epoch-based virtual wall
// time (about 1.77e9 s), so clock() read about 1.77e15.
//
// With the argument "values" it also prints the CPU times it read. Under
// Hermit they are deterministic, so two runs print the same bytes.

#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/times.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define NANOS_PER_SEC 1000000000LL
#define NANOS_PER_MICRO 1000LL

static int checks;
static int failures;
static int print_values;

static void check(const char* name, int ok) {
  checks++;
  if (!ok) {
    failures++;
  }
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
}

static void value(const char* name, int64_t nanos) {
  if (print_values) {
    printf("value %s %" PRId64 "\n", name, nanos);
  }
}

static int64_t timespec_ns(const struct timespec* t) {
  return (int64_t)t->tv_sec * NANOS_PER_SEC + t->tv_nsec;
}

static int64_t timeval_ns(const struct timeval* t) {
  return (int64_t)t->tv_sec * NANOS_PER_SEC + (int64_t)t->tv_usec * NANOS_PER_MICRO;
}

// The clock's reading in nanoseconds, or -1 with errno set.
static int64_t clock_ns(clockid_t id) {
  struct timespec t;
  if (clock_gettime(id, &t) != 0) {
    return -1;
  }
  return timespec_ns(&t);
}

static pid_t current_tid(void) {
  return (pid_t)syscall(SYS_gettid);
}

// MAKE_THREAD_CPUCLOCK(tid, CPUCLOCK_SCHED), include/linux/posix-timers.h.
static clockid_t thread_sched_clock(pid_t tid) {
  return (clockid_t)((~(unsigned)tid << 3) | 4 | 2);
}

// Work that retires conditional branches in proportion to `rounds`.
static uint64_t burn(uint64_t rounds) {
  volatile uint64_t acc = 0;
  for (uint64_t i = 0; i < rounds; i++) {
    acc += (i & 1) ? i : acc >> 3;
  }
  return acc;
}

static void check_basic_clocks(void) {
  int64_t thread = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  int64_t process = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  int64_t realtime = clock_ns(CLOCK_REALTIME);
  value("start-thread", thread);
  value("start-process", process);
  check(
      "thread-and-process-clocks-read-cpu-time-not-the-wall-clock",
      thread >= 0 && process >= thread && process < 10 * NANOS_PER_SEC &&
          realtime > 1000 * NANOS_PER_SEC);
}

static void check_advance_with_work(void) {
  int64_t t0 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  burn(1000000);
  int64_t t1 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  burn(10000000);
  int64_t t2 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  value("short-burn", t1 - t0);
  value("long-burn", t2 - t1);
  check("thread-clock-advances-with-work", t1 > t0 && t2 - t1 > t1 - t0);
}

static void check_repeated_reads(void) {
  int ordered = 1;
  int64_t first_thread = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  int64_t first_process = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  int64_t thread = first_thread;
  int64_t process = first_process;
  for (int i = 0; i < 200; i++) {
    int64_t next_thread = clock_ns(CLOCK_THREAD_CPUTIME_ID);
    int64_t next_process = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
    ordered &= next_thread >= thread && next_process >= process;
    thread = next_thread;
    process = next_process;
  }
  check(
      "repeated-reads-never-go-back-and-advance",
      ordered && thread > first_thread && process > first_process);
}

static void check_own_clock_ids(void) {
  clockid_t self_clock;
  clockid_t process_clock;
  clockid_t pid_zero_clock;
  int ids_ok = pthread_getcpuclockid(pthread_self(), &self_clock) == 0 &&
      clock_getcpuclockid(getpid(), &process_clock) == 0 &&
      clock_getcpuclockid(0, &pid_zero_clock) == 0;
  check("own-cpu-clock-ids-are-valid", ids_ok);
  if (!ids_ok) {
    return;
  }
  int64_t a = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  int64_t b = clock_ns(self_clock);
  int64_t c = clock_ns(thread_sched_clock(current_tid()));
  int64_t d = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  check(
      "own-thread-clock-ids-read-the-thread-clock",
      a >= 0 && a <= b && b <= c && c <= d);
  int64_t p0 = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  int64_t p1 = clock_ns(process_clock);
  int64_t p2 = clock_ns(pid_zero_clock);
  int64_t p3 = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  check(
      "own-process-clock-ids-read-the-process-clock",
      p0 >= 0 && p0 <= p1 && p1 <= p2 && p2 <= p3);
}

static void check_other_cpu_views(void) {
  // clock() is CLOCK_PROCESS_CPUTIME_ID in CLOCKS_PER_SEC (1e6) units.
  int64_t before = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  clock_t ticks = clock();
  int64_t after = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  value("clock", (int64_t)ticks);
  check(
      "clock-is-process-cpu-time-in-microseconds",
      CLOCKS_PER_SEC == 1000000 && ticks >= 0 &&
          (int64_t)ticks >= before / NANOS_PER_MICRO &&
          (int64_t)ticks <= after / NANOS_PER_MICRO &&
          (int64_t)ticks < 10 * CLOCKS_PER_SEC);

  // getrusage's user and system time add up to the matching clock; each is
  // truncated to whole microseconds.
  struct rusage self;
  struct rusage thread;
  int64_t p0 = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  int self_rc = getrusage(RUSAGE_SELF, &self);
  int64_t p1 = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  int64_t t0 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  int thread_rc = getrusage(RUSAGE_THREAD, &thread);
  int64_t t1 = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  int64_t self_ns = timeval_ns(&self.ru_utime) + timeval_ns(&self.ru_stime);
  int64_t thread_ns =
      timeval_ns(&thread.ru_utime) + timeval_ns(&thread.ru_stime);
  check(
      "getrusage-self-matches-the-process-clock",
      self_rc == 0 && self_ns >= p0 - 2 * NANOS_PER_MICRO && self_ns <= p1);
  check(
      "getrusage-thread-matches-the-thread-clock",
      thread_rc == 0 && thread_ns >= t0 - 2 * NANOS_PER_MICRO &&
          thread_ns <= t1);

  // times reports clock ticks, each field truncated.
  long hz = sysconf(_SC_CLK_TCK);
  struct tms usage;
  int64_t q0 = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  clock_t times_rc = times(&usage);
  int64_t q1 = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  int64_t tick_ns = hz > 0 ? NANOS_PER_SEC / hz : 0;
  int64_t used_ticks = (int64_t)usage.tms_utime + (int64_t)usage.tms_stime;
  check(
      "times-matches-the-process-clock",
      times_rc != (clock_t)-1 && tick_ns > 0 &&
          used_ticks >= q0 / tick_ns - 2 && used_ticks <= q1 / tick_ns);
}

struct new_thread_reading {
  int64_t first;
};

static void* read_new_thread_clock(void* arg) {
  struct new_thread_reading* reading = arg;
  reading->first = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  return NULL;
}

static void check_new_thread_starts_small(void) {
  struct new_thread_reading reading = {-1};
  int64_t creator = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  pthread_t thread;
  int created =
      pthread_create(&thread, NULL, read_new_thread_clock, &reading) == 0;
  if (created) {
    pthread_join(thread, NULL);
  }
  value("new-thread-first", reading.first);
  check(
      "new-thread-clock-starts-near-zero",
      created && reading.first >= 0 && reading.first < creator &&
          reading.first < 5 * NANOS_PER_SEC / 1000);
}

struct worker {
  pthread_mutex_t lock;
  pthread_cond_t changed;
  int stage;
  pid_t tid;
  int64_t own_reading;
};

static void wait_for_stage(struct worker* w, int stage) {
  pthread_mutex_lock(&w->lock);
  while (w->stage < stage) {
    pthread_cond_wait(&w->changed, &w->lock);
  }
  pthread_mutex_unlock(&w->lock);
}

static void set_stage(struct worker* w, int stage) {
  pthread_mutex_lock(&w->lock);
  w->stage = stage;
  pthread_cond_broadcast(&w->changed);
  pthread_mutex_unlock(&w->lock);
}

// Burns, reads its own clock, reports, then blocks until released.
static void* run_worker(void* arg) {
  struct worker* w = arg;
  w->tid = current_tid();
  burn(3000000);
  w->own_reading = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  set_stage(w, 1);
  wait_for_stage(w, 2);
  return NULL;
}

// The state letter of thread `tid` of this process, from its stat file, or
// '?' if it cannot be read.
static char thread_state(pid_t tid) {
  char path[64];
  char line[512];
  snprintf(path, sizeof(path), "/proc/self/task/%d/stat", (int)tid);
  FILE* file = fopen(path, "r");
  if (file == NULL) {
    return '?';
  }
  size_t length = fread(line, 1, sizeof(line) - 1, file);
  fclose(file);
  line[length] = '\0';
  char* name_end = strrchr(line, ')');
  return name_end != NULL && name_end[1] == ' ' ? name_end[2] : '?';
}

// Waits until the worker sleeps in the kernel: its stat says S and its
// clock stands still across a 1 ms sleep of this thread. Reporting stage 1
// does not mean it has reached its wait yet.
static int64_t wait_until_asleep(clockid_t clock, pid_t tid) {
  for (int attempt = 0; attempt < 1000; attempt++) {
    char before_state = thread_state(tid);
    int64_t before = clock_ns(clock);
    const struct timespec pause = {0, 1000000};
    nanosleep(&pause, NULL);
    int64_t after = clock_ns(clock);
    if (before_state == 'S' && thread_state(tid) == 'S' && before == after &&
        before >= 0) {
      return after;
    }
  }
  return -1;
}

static void check_sibling_clocks(void) {
  struct worker w = {
      PTHREAD_MUTEX_INITIALIZER, PTHREAD_COND_INITIALIZER, 0, 0, -1};
  pthread_t thread;
  if (pthread_create(&thread, NULL, run_worker, &w) != 0) {
    check("sibling-thread-starts", 0);
    return;
  }
  wait_for_stage(&w, 1);
  clockid_t sibling_clock;
  int id_ok = pthread_getcpuclockid(thread, &sibling_clock) == 0;
  int64_t seen = id_ok ? wait_until_asleep(sibling_clock, w.tid) : -1;
  int64_t by_tid = clock_ns(thread_sched_clock(w.tid));
  burn(3000000);
  int64_t seen_again = id_ok ? clock_ns(sibling_clock) : -1;
  int64_t own = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  int64_t process = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  value("sibling", seen);
  check(
      "sibling-clock-reads-its-cpu-time",
      id_ok && seen >= w.own_reading && w.own_reading > 0 && by_tid == seen);
  check("blocked-sibling-clock-stands-still", seen >= 0 && seen_again == seen);
  check(
      "process-clock-covers-every-thread",
      seen >= 0 && own >= 0 && process >= own + seen);
  set_stage(&w, 2);
  pthread_join(thread, NULL);
}

struct exiting {
  pid_t tid;
  int64_t last;
};

static void* run_and_exit(void* arg) {
  struct exiting* e = arg;
  e->tid = current_tid();
  burn(3000000);
  e->last = clock_ns(CLOCK_THREAD_CPUTIME_ID);
  return NULL;
}

static void check_exited_thread(void) {
  struct exiting e = {0, -1};
  int64_t before = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  pthread_t thread;
  if (pthread_create(&thread, NULL, run_and_exit, &e) != 0) {
    check("exiting-thread-starts", 0);
    return;
  }
  pthread_join(thread, NULL);
  int64_t after = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  check(
      "exited-thread-time-stays-in-the-process-clock",
      e.last > 0 && after - before >= e.last);
  // The joiner wakes when the exiting thread clears its tid, which Linux
  // does shortly before it releases the task; retry until it is gone.
  int gone = 0;
  for (int attempt = 0; attempt < 1000 && !gone; attempt++) {
    errno = 0;
    gone = clock_ns(thread_sched_clock(e.tid)) == -1 && errno == EINVAL;
    if (!gone) {
      const struct timespec pause = {0, 1000000};
      nanosleep(&pause, NULL);
    }
  }
  check("exited-thread-id-names-no-clock", gone);
}

static void check_fork_child(void) {
  int64_t parent = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    int64_t thread = clock_ns(CLOCK_THREAD_CPUTIME_ID);
    int64_t process = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
    _exit(thread >= 0 && process >= thread && process < parent ? 0 : 1);
  }
  int status = 0;
  int waited = child > 0 && waitpid(child, &status, 0) == child;
  check(
      "fork-child-clocks-start-afresh",
      waited && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

struct exited_leader {
  pthread_t leader;
  clockid_t clock;
  int64_t last;
};

static void* read_exited_leader(void* arg) {
  struct exited_leader* leader = arg;
  // Returns once the kernel has cleared the leader's tid at its exit.
  pthread_join(leader->leader, NULL);
  int64_t first = clock_ns(leader->clock);
  burn(100000);
  int64_t second = clock_ns(leader->clock);
  _exit(first > 0 && first >= leader->last && second == first ? 0 : 1);
}

// A process whose leader exits before its other thread: the leader stays a
// zombie while the process lives, and its thread clock, read by the other
// thread, keeps the leader's final CPU time.
static void check_exited_leader(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    static struct exited_leader leader;
    leader.leader = pthread_self();
    leader.clock = thread_sched_clock(getpid());
    burn(100000);
    leader.last = clock_ns(leader.clock);
    pthread_t reader;
    if (pthread_create(&reader, NULL, read_exited_leader, &leader) != 0) {
      _exit(2);
    }
    pthread_exit(NULL);
  }
  int status = 0;
  int waited = child > 0 && waitpid(child, &status, 0) == child;
  check(
      "an-exited-leaders-clock-keeps-its-final-time",
      waited && WIFEXITED(status) && WEXITSTATUS(status) == 0);
}

static void check_clock_nanosleep(void) {
  const struct timespec request = {0, 1000};
  check(
      "glibc-refuses-a-thread-clock-sleep",
      clock_nanosleep(CLOCK_THREAD_CPUTIME_ID, 0, &request, NULL) == EINVAL);
  errno = 0;
  long raw = syscall(
      SYS_clock_nanosleep, CLOCK_THREAD_CPUTIME_ID, 0, &request, NULL);
  check(
      "kernel-has-no-sleep-for-clock-thread-cputime-id",
      raw == -1 && errno == EOPNOTSUPP);
  errno = 0;
  raw = syscall(
      SYS_clock_nanosleep,
      thread_sched_clock(current_tid()),
      0,
      &request,
      NULL);
  check(
      "kernel-refuses-a-sleep-on-the-own-thread-clock",
      raw == -1 && errno == EINVAL);

  const struct timespec passed = {0, 1};
  check(
      "passed-process-clock-deadline-returns-at-once",
      clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME, &passed, NULL) ==
          0);
}

static int finish(void) {
  printf("cpu_time_clocks: %d checks, %d failed\n", checks, failures);
  return failures == 0 ? 0 : 1;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IOLBF, 0);
  if (argc >= 6 && strcmp(argv[1], "after-exec") == 0) {
    // The image an exec replaced passed its last thread- and process-clock
    // readings and its counts.
    print_values = strcmp(argv[2], "values") == 0;
    int64_t thread_before = strtoll(argv[3], NULL, 10);
    int64_t process_before = strtoll(argv[4], NULL, 10);
    sscanf(argv[5], "%d/%d", &checks, &failures);
    int64_t thread_after = clock_ns(CLOCK_THREAD_CPUTIME_ID);
    int64_t process_after = clock_ns(CLOCK_PROCESS_CPUTIME_ID);
    check(
        "exec-keeps-the-thread-clock",
        thread_after >= thread_before && thread_before > 0);
    check(
        "exec-keeps-the-process-clock",
        process_after >= process_before && process_before > 0);
    return finish();
  }
  print_values = argc >= 2 && strcmp(argv[1], "values") == 0;

  check_basic_clocks();
  check_advance_with_work();
  check_repeated_reads();
  check_own_clock_ids();
  check_other_cpu_views();
  check_new_thread_starts_small();
  check_sibling_clocks();
  check_exited_thread();
  check_fork_child();
  check_exited_leader();
  check_clock_nanosleep();

  char thread_before[32];
  char process_before[32];
  char counts[32];
  snprintf(
      thread_before,
      sizeof(thread_before),
      "%" PRId64,
      clock_ns(CLOCK_THREAD_CPUTIME_ID));
  snprintf(
      process_before,
      sizeof(process_before),
      "%" PRId64,
      clock_ns(CLOCK_PROCESS_CPUTIME_ID));
  snprintf(counts, sizeof(counts), "%d/%d", checks, failures);
  fflush(stdout);
  char* const args[] = {
      argv[0],
      "after-exec",
      print_values ? "values" : "plain",
      thread_before,
      process_before,
      counts,
      NULL};
  execv("/proc/self/exe", args);
  check("exec-keeps-the-thread-clock", 0);
  check("exec-keeps-the-process-clock", 0);
  return finish();
}
