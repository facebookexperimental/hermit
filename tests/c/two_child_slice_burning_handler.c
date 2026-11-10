#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <poll.h>

/* Two child exits straddling one slice-burning handler return (Hermit
 * https://github.com/rrnewton/hermit/pull/5259). Like
 * two_child_after_handler.c, except the first handler burns CPU past
 * at least one timeslice before returning, and the creator makes no
 * syscall between its return and the second exit: a delivery frame
 * that only a timeslice boundary can retire decides the second exit
 * if the boundary clears the sigreturn report without popping it.
 *
 * Both children are created before M's wait. W1 first positively observes M
 * sleeping in ppoll (native /proc syscall number, where available), then lets
 * C1 exit and waits in read. C1 additionally waits for W1 to be asleep after
 * that handshake. M's caught SIGCHLD handler must have returned AND ppoll
 * must have returned EINTR before M releases C2. There is no mask-changing
 * call or new wait between that first return and the second SIGCHLD: M only
 * reads the monotonic clock and yields. W1 stays eligible, with no pending
 * signal and an empty mask, in read.
 *
 * Native Linux's complete_signal first tries the process leader M. Its own
 * mask and the temporary ppoll mask are both empty, so M receives both child
 * notifications. A scheduler delivery frame that survives the first handler
 * instead remembers SIGCHLD's automatic handler block and can choose W1 for
 * the second notification. The positive handshake excludes coalescing the
 * two exits and excludes a second exit before the first sigreturn.
 *
 * Every wait has a wall-clock deadline; deadlines are failure bounds, never
 * evidence that a state was reached. A failed prerequisite returns 3. A
 * confirmed receiver mismatch is printed and returns 1. No comparison is
 * relaxed for Hermit. As in process_signal_target.c, Hermit's virtual /proc
 * may omit syscall; only ENOENT permits using its positive sleeping-state
 * observation instead. Native must expose and match the syscall number.
 */

struct shared {
  _Atomic int first_gate;
  _Atomic int second_gate;
  _Atomic int main_entering;
  _Atomic int worker_tid;
};

static struct shared *shared;
static pid_t parent_pid;
static pid_t main_tid;
static pid_t children[2];
static int worker_pipe[2];
static _Thread_local int identity; /* M=0, W1=1; children never handle SIGCHLD. */
static _Atomic int receivers[2] = {-1, -1};
static int64_t deadline;

static int64_t now_ms(void) {
  struct timespec ts;
  if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) _exit(3);
  return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void unconfirmed(const char *reason) {
  fprintf(stderr, "UNCONFIRMED: %s\n", reason);
  _exit(3);
}

static void bounded_yield(void) {
  if (now_ms() >= deadline) unconfirmed("positive handshake timed out");
  sched_yield();
}

static int sleeping(pid_t tid) {
  char path[96], buf[512];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/stat", parent_pid, tid);
  FILE *f = fopen(path, "r");
  if (!f) return 0;
  char *line = fgets(buf, sizeof(buf), f);
  fclose(f);
  char *end = line ? strrchr(buf, ')') : NULL;
  return end && strchr("SDtT", end[2]) != NULL;
}

static int waiting_in(pid_t tid, long number) {
  if (!sleeping(tid)) return 0;
  char path[96], buf[128];
  snprintf(path, sizeof(path), "/proc/%d/task/%d/syscall", parent_pid, tid);
  FILE *f = fopen(path, "r");
  if (!f && errno == ENOENT) return 1;
  if (!f) unconfirmed("sleeping thread syscall file unreadable");
  char *line = fgets(buf, sizeof(buf), f);
  fclose(f);
  if (!line) unconfirmed("sleeping thread syscall file empty");
  char *end;
  long got = strtol(buf, &end, 10);
  return end != buf && got == number;
}

static void on_child(int signo, siginfo_t *info, void *context) {
  (void)signo;
  (void)context;
  int saved_errno = errno;
  /* Burn CPU in place, past at least one timeslice, before returning:
   * no syscall, so only a slice boundary can observe this handler. */
  for (volatile unsigned long i = 0; i < 600000000UL; ++i) {
  }
  for (int i = 0; i < 2; ++i) {
    if (info->si_pid == children[i]) {
      atomic_store_explicit(&receivers[i], identity, memory_order_release);
    }
  }
  errno = saved_errno;
}

static void *worker(void *unused) {
  (void)unused;
  identity = 1;
  atomic_store(&shared->worker_tid, (int)syscall(SYS_gettid));
  while (!atomic_load(&shared->main_entering)) bounded_yield();
  while (!waiting_in(main_tid, SYS_ppoll)) bounded_yield();
  atomic_store_explicit(&shared->first_gate, 1, memory_order_release);
  /* After publishing first_gate this read is W1's only sleeping operation.
   * Thus C1's subsequent positive sleeping observation confirms this read. */
  char byte;
  for (;;) {
    ssize_t n = read(worker_pipe[0], &byte, 1);
    if (n == 1) return NULL;
    if (n < 0 && errno == EINTR) continue;
    unconfirmed("worker read failed");
  }
}

static pid_t make_child(int index) {
  pid_t child = fork();
  if (child < 0) unconfirmed("fork failed");
  if (child == 0) {
    _Atomic int *gate = index == 0 ? &shared->first_gate : &shared->second_gate;
    while (!atomic_load_explicit(gate, memory_order_acquire)) bounded_yield();
    if (index == 0) {
      pid_t worker_tid = atomic_load(&shared->worker_tid);
      if (worker_tid <= 0) unconfirmed("first gate preceded worker registration");
      while (!sleeping(worker_tid)) bounded_yield();
    }
    _exit(0);
  }
  return child;
}

int main(void) {
  deadline = now_ms() + 20000;
  parent_pid = getpid();
  main_tid = (pid_t)syscall(SYS_gettid);
  shared = mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (shared == MAP_FAILED || pipe(worker_pipe) != 0) unconfirmed("setup failed");
  sigset_t empty;
  sigemptyset(&empty);
  if (pthread_sigmask(SIG_SETMASK, &empty, NULL) != 0) unconfirmed("initial mask");
  struct sigaction action = {.sa_sigaction = on_child, .sa_flags = SA_SIGINFO};
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGCHLD, &action, NULL) != 0) unconfirmed("SIGCHLD action");
  children[0] = make_child(0);
  children[1] = make_child(1);
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL) != 0) unconfirmed("pthread_create");
  while (!atomic_load(&shared->worker_tid)) bounded_yield();
  atomic_store(&shared->main_entering, 1);
  struct timespec timeout = {.tv_sec = 10, .tv_nsec = 0};
  int result = ppoll(NULL, 0, &timeout, &empty);
  int first = atomic_load_explicit(&receivers[0], memory_order_acquire);
  if (result != -1 || errno != EINTR || first != 0)
    unconfirmed("M's first SIGCHLD handler and ppoll EINTR not confirmed");
  /* This store is after both the actual handler's return and ppoll's return.
   * From here to the second observation, M begins no wait and changes no mask.
   * The failure bound restarts here: the burn above is CPU, and under a
   * branch-accounted clock it alone would eat a from-start bound. */
  deadline = now_ms() + 20000;
  atomic_store_explicit(&shared->second_gate, 1, memory_order_release);
  /* No syscall between the handler's return and this observation, so
   * only a slice boundary (or a later choice in the kernel) can retire
   * state the return left behind. The spin is capped: past the cap
   * (far past any PMU timeslice) it yields, so a configuration without
   * preemption (no PMU, or --max-timeslice=disabled) cannot starve
   * the second child. */
  /* Iteration-budgeted, not wall-clocked: under a branch-accounted
   * clock the burn above already ate any from-start wall bound. The
   * syscall-free window (2M iterations) is far past a PMU timeslice;
   * past it the loop yields, so a no-preemption configuration degrades
   * to the ordinary two-child case instead of starving the child. */
  unsigned long budget = 0;
  unsigned long spin_past_cap = 0;
  while (atomic_load_explicit(&receivers[1], memory_order_acquire) < 0) {
    for (volatile int i = 0; i < 1000; ++i) {
    }
    if (++budget == 2000UL) {
      bounded_yield();
      budget = 0;
    }
    if (budget == 0 && ++spin_past_cap > 1000000UL)
      unconfirmed("second handler observation expired");
  }
  int second = atomic_load(&receivers[1]);
  printf("first=%s second=%s\n", first == 0 ? "M" : "W1", second == 0 ? "M" : "W1");
  if (write(worker_pipe[1], "x", 1) != 1 || pthread_join(thread, NULL) != 0)
    unconfirmed("worker cleanup failed");
  for (int i = 0; i < 2; ++i) {
    int status;
    if (waitpid(children[i], &status, 0) != children[i] ||
        !WIFEXITED(status) || WEXITSTATUS(status) != 0)
      unconfirmed("child prerequisite failed");
  }
  return first == 0 && second == 0 ? 0 : 1;
}
