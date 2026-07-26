#define _GNU_SOURCE

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define WORKERS 4
#define THREAD_ITERATIONS 1000
#define SIGNAL_ITERATIONS 200
#define FORK_ITERATIONS 8

struct worker {
  pthread_t thread;
  uint64_t seed;
};

struct signal_target {
  pid_t pid;
  pid_t tid;
};

static pthread_barrier_t start_barrier;
static atomic_int signal_count;
static atomic_int signal_sender_ready;
static struct signal_target signal_target;

static void fail(const char *operation) {
  fprintf(stderr, "%s: %s\n", operation, strerror(errno));
  exit(1);
}

static void syscall_burst(unsigned int iterations, uint64_t state) {
  for (unsigned int iteration = 0; iteration < iterations; ++iteration) {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    (void)syscall(SYS_getpid);
    if ((state & 7) == 0) {
      (void)syscall(SYS_sched_yield);
    }
  }
}

static void *worker_main(void *argument) {
  struct worker *worker = argument;
  int result = pthread_barrier_wait(&start_barrier);
  if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
    errno = result;
    fail("pthread_barrier_wait");
  }
  syscall_burst(THREAD_ITERATIONS, worker->seed);
  return NULL;
}

static void start_workers(struct worker workers[WORKERS], uint64_t seed) {
  int result = pthread_barrier_init(&start_barrier, NULL, WORKERS + 1);
  if (result != 0) {
    errno = result;
    fail("pthread_barrier_init");
  }
  for (unsigned int index = 0; index < WORKERS; ++index) {
    workers[index].seed = seed ^ (uint64_t)(index + 1);
    result = pthread_create(&workers[index].thread, NULL, worker_main,
                            &workers[index]);
    if (result != 0) {
      errno = result;
      fail("pthread_create");
    }
  }
  result = pthread_barrier_wait(&start_barrier);
  if (result != 0 && result != PTHREAD_BARRIER_SERIAL_THREAD) {
    errno = result;
    fail("pthread_barrier_wait");
  }
}

static void join_workers(struct worker workers[WORKERS]) {
  for (unsigned int index = 0; index < WORKERS; ++index) {
    int result = pthread_join(workers[index].thread, NULL);
    if (result != 0) {
      errno = result;
      fail("pthread_join");
    }
  }
  int result = pthread_barrier_destroy(&start_barrier);
  if (result != 0) {
    errno = result;
    fail("pthread_barrier_destroy");
  }
}

static void run_threads(uint64_t seed) {
  struct worker workers[WORKERS];
  start_workers(workers, seed);
  join_workers(workers);
}

static void wait_child(pid_t child) {
  int status = 0;
  pid_t result;
  do {
    result = waitpid(child, &status, 0);
  } while (result < 0 && errno == EINTR);
  if (result != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    errno = result < 0 ? errno : ECHILD;
    fail("waitpid");
  }
}

static void signal_handler(int signal_number) {
  (void)signal_number;
  (void)syscall(SYS_gettid);
  atomic_fetch_add_explicit(&signal_count, 1, memory_order_release);
}

static void *signal_sender_main(void *argument) {
  (void)argument;
  while (!atomic_load_explicit(&signal_sender_ready, memory_order_acquire)) {
    __asm__ volatile("pause");
  }
  for (int expected = 1; expected <= SIGNAL_ITERATIONS; ++expected) {
    if (syscall(SYS_tgkill, signal_target.pid, signal_target.tid, SIGUSR1) != 0) {
      atomic_store_explicit(&signal_count, SIGNAL_ITERATIONS,
                            memory_order_release);
      return (void *)(uintptr_t)1;
    }
    while (atomic_load_explicit(&signal_count, memory_order_acquire) < expected) {
      __asm__ volatile("pause");
    }
  }
  return NULL;
}

static pthread_t start_signal_sender(void) {
  struct sigaction action = {0};
  action.sa_handler = signal_handler;
  action.sa_flags = SA_RESTART;
  if (sigemptyset(&action.sa_mask) != 0 ||
      sigaction(SIGUSR1, &action, NULL) != 0) {
    fail("sigaction");
  }

  if (!atomic_is_lock_free(&signal_count) ||
      !atomic_is_lock_free(&signal_sender_ready)) {
    errno = ENOTSUP;
    fail("atomic_is_lock_free");
  }
  sigset_t blocked;
  if (sigemptyset(&blocked) != 0 || sigaddset(&blocked, SIGUSR1) != 0) {
    fail("sigset");
  }
  int mask_result = pthread_sigmask(SIG_BLOCK, &blocked, NULL);
  if (mask_result != 0) {
    errno = mask_result;
    fail("pthread_sigmask");
  }

  signal_target.pid = getpid();
  signal_target.tid = (pid_t)syscall(SYS_gettid);
  atomic_store_explicit(&signal_sender_ready, 0, memory_order_relaxed);
  pthread_t sender;
  int result = pthread_create(&sender, NULL, signal_sender_main, NULL);
  if (result != 0) {
    errno = result;
    fail("pthread_create");
  }
  atomic_store_explicit(&signal_sender_ready, 1, memory_order_release);
  return sender;
}

static void wait_for_signals(void) {
  sigset_t unblocked;
  if (sigemptyset(&unblocked) != 0) {
    fail("sigemptyset");
  }
  for (int expected = 1; expected <= SIGNAL_ITERATIONS; ++expected) {
    while (atomic_load_explicit(&signal_count, memory_order_acquire) <
           expected) {
      struct timespec timeout = {.tv_sec = 1};
      int result = ppoll(NULL, 0, &timeout, &unblocked);
      if (result == 0) {
        continue;
      }
      if (result != -1 || errno != EINTR) {
        fail("ppoll");
      }
    }
  }
}

static void unblock_signal(void) {
  sigset_t blocked;
  if (sigemptyset(&blocked) != 0 || sigaddset(&blocked, SIGUSR1) != 0) {
    fail("sigset");
  }
  int result = pthread_sigmask(SIG_UNBLOCK, &blocked, NULL);
  if (result != 0) {
    errno = result;
    fail("pthread_sigmask");
  }
}

static void finish_signal_sender(pthread_t sender) {
  void *result = NULL;
  int join_result = pthread_join(sender, &result);
  if (join_result != 0 || result != NULL) {
    errno = join_result != 0 ? join_result : EIO;
    fail("pthread_join");
  }
  int observed = atomic_load_explicit(&signal_count, memory_order_acquire);
  if (observed != SIGNAL_ITERATIONS) {
    fprintf(stderr, "expected %d signals, observed %d\n", SIGNAL_ITERATIONS,
            observed);
    exit(1);
  }
  unblock_signal();
}

static void run_signal_storm(void) {
  atomic_store_explicit(&signal_count, 0, memory_order_relaxed);
  pthread_t sender = start_signal_sender();
  wait_for_signals();
  finish_signal_sender(sender);
}

static void run_fork_stress(void) {
  for (unsigned int index = 0; index < FORK_ITERATIONS; ++index) {
    pid_t child = fork();
    if (child < 0) {
      fail("fork");
    }
    if (child == 0) {
      (void)syscall(SYS_getpid);
      _exit(0);
    }
    wait_child(child);
  }
}

static void run_chaos(uint64_t seed) {
  atomic_store_explicit(&signal_count, 0, memory_order_relaxed);
  pthread_t sender = start_signal_sender();
  struct worker workers[WORKERS];
  start_workers(workers, seed);
  run_fork_stress();
  join_workers(workers);
  wait_for_signals();
  finish_signal_sender(sender);
}

static void run_phased_chaos(uint64_t seed) {
  run_signal_storm();
  run_threads(seed);
  run_fork_stress();
}

static uint64_t parse_seed(const char *value) {
  char *end = NULL;
  errno = 0;
  unsigned long long seed = strtoull(value, &end, 10);
  if (errno != 0 || end == value || *end != '\0') {
    fprintf(stderr, "invalid seed: %s\n", value);
    exit(2);
  }
  return (uint64_t)seed;
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "threads") == 0) {
    run_threads(1);
    puts("threads-ok");
    return 0;
  }
  if (argc == 2 && strcmp(argv[1], "signals") == 0) {
    run_signal_storm();
    puts("signals-ok");
    return 0;
  }
  if (argc == 2 && strcmp(argv[1], "fork") == 0) {
    run_fork_stress();
    puts("fork-ok");
    return 0;
  }
  if (argc == 3 && strcmp(argv[1], "chaos-verify") == 0) {
    run_phased_chaos(parse_seed(argv[2]));
    puts("chaos-verify-ok");
    return 0;
  }
  if (argc == 3 && strcmp(argv[1], "chaos") == 0) {
    run_chaos(parse_seed(argv[2]));
    puts("chaos-ok");
    return 0;
  }
  fprintf(stderr,
          "usage: %s threads|signals|fork|chaos[-verify] SEED\n", argv[0]);
  return 2;
}
