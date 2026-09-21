#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int worker_receives;
static _Atomic int receiver_ready;
static _Atomic int handler_active;
static _Atomic int receiver_tid;
static _Atomic int signal_count;

static void alarm_handler(int signal, siginfo_t *info, void *context) {
  (void)context;
  if (signal != SIGALRM || info->si_code != SI_KERNEL ||
      syscall(SYS_gettid) != atomic_load(&receiver_tid) ||
      atomic_fetch_add(&signal_count, 1) != 0)
    _exit(87);
  const char leader[] = "timer-retirement: leader handler\n";
  const char worker[] = "timer-retirement: worker handler\n";
  const char *text = worker_receives ? worker : leader;
  const size_t length = worker_receives ? sizeof(worker) - 1 : sizeof(leader) - 1;
  if (write(STDOUT_FILENO, text, length) != (ssize_t)length) _exit(88);
  atomic_store(&handler_active, 1);
  // Stay in a real guest signal frame while timestamp callbacks let the
  // deterministic scheduler grant the sibling's real exit_group request.
  for (;;) {
    unsigned int lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
  }
}

static void prepare_receiver(void) {
  atomic_store(&receiver_tid, (int)syscall(SYS_gettid));
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGALRM);
  if (pthread_sigmask(SIG_UNBLOCK, &mask, NULL)) _exit(81);
  atomic_store(&receiver_ready, 1);
}
static void await_signal(void) {
  struct timespec wait = {.tv_sec = 1};
  struct timespec remaining = {0};
  (void)nanosleep(&wait, &remaining);
  _exit(89);  // The signal handler must be consumed by sibling exit_group.
}
static void issue_group_exit(int status) {
  while (!atomic_load(&handler_active)) sched_yield();
  syscall(SYS_exit_group, status);
  _exit(90);
}
static void *worker(void *unused) {
  (void)unused;
  if (worker_receives) {
    prepare_receiver();
    await_signal();
  } else {
    issue_group_exit(95);
  }
  return NULL;
}
int main(int argc, char **argv) {
  if (argc != 2 || (argv[1][0] != '0' && argv[1][0] != '1') || argv[1][1]) return 80;
  worker_receives = argv[1][0] == '1';
  sigset_t mask;
  sigemptyset(&mask);
  sigaddset(&mask, SIGALRM);
  if (pthread_sigmask(SIG_BLOCK, &mask, NULL)) return 81;
  struct sigaction action = {0};
  action.sa_sigaction = alarm_handler;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGALRM, &action, NULL)) return 82;
  pthread_t thread;
  if (pthread_create(&thread, NULL, worker, NULL)) return 83;
  if (!worker_receives) prepare_receiver();
  while (!atomic_load(&receiver_ready)) sched_yield();
  struct itimerval timer = {0};
  timer.it_value.tv_usec = 1;
  if (setitimer(ITIMER_REAL, &timer, NULL)) return 84;
  if (worker_receives) issue_group_exit(17);
  await_signal();
  return 91;
}
