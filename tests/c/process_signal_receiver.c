/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Which thread of a multi-threaded process takes a process-directed signal
 * (https://github.com/rrnewton/hermit/issues/4034). Linux queues it on the
 * process's shared queue and complete_signal (kernel/signal.c) wakes the
 * thread the call names (the leader, for a process id) if it does not block
 * the signal, and otherwise the first thread that does not; with none, the
 * signal stays queued until a thread unblocks it. Each mode prints which
 * thread ran the handler and the siginfo it saw, so a run under Hermit can be
 * compared with a native one:
 *
 *   term        a parent's kill(child, SIGTERM) to a four-thread child whose
 *               threads all leave SIGTERM unblocked: the main thread takes it.
 *   term-one    the same, with SIGTERM blocked in every thread but worker 2,
 *               which takes it.
 *   term-queued the same, with SIGTERM blocked in every thread: it stays
 *               pending (sigpending shows it) until worker 2 unblocks it and
 *               takes it.
 *   term-busy   the same as term, but worker 1 is runnable and spinning
 *               instead of parked in pause(): the main thread still takes
 *               it, and the child exits before the spinning worker prints,
 *               so the chosen thread must be resumed before any other
 *               runnable thread.
 *   usr1-after-failed
 *               in the child, the main thread and workers 1 and 3 block
 *               SIGUSR1 and worker 2 does not. The parent's
 *               pidfd_send_signal of SIGUSR1 with a siginfo whose si_code is
 *               SI_USER fails with EPERM before anything is sent (a code only
 *               the target itself may use), so it moves no curr_target.
 *               Worker 1 then unblocks SIGUSR1, and the parent's
 *               kill(child, SIGUSR1) names the main thread, which blocks it:
 *               the walk from curr_target, still the main thread, reaches
 *               worker 1, which takes it, ahead of worker 2.
 *   usr1-self   worker 1's kill(getpid(), SIGUSR1) in a four-thread process
 *               whose threads all leave it unblocked: the main thread takes it.
 *   term-after-walk
 *               in the child, the main thread and worker 3 block SIGUSR1 and
 *               SIGTERM, worker 1 blocks SIGUSR1 alone and worker 2 neither.
 *               Worker 2's kill(getpid(), SIGUSR1) names the main thread,
 *               which blocks it, so the walk from curr_target (the main
 *               thread) reaches worker 2, which takes it and becomes
 *               curr_target. The parent's kill(child, SIGTERM) names the main
 *               thread, which blocks it too, and the walk starts at worker 2,
 *               which takes it, ahead of worker 1.
 *   segv-pidfd  a two-thread child whose worker sends its own process SIGSEGV
 *               with pidfd_send_signal, at its default disposition: the whole
 *               child dies of SIGSEGV.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define WORKERS 3

/* What a handler reports to the main thread. */
struct report {
  int role; /* 0: the main thread, n: worker n */
  int code;
  int pid;
};

static int report_pipe[2];
static __thread int role;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int ready;
static int go;
static sigset_t worker_masks[WORKERS + 1];
/* The worker that sends SIGUSR1 to its own process once told to (usr1-self). */
static int usr1_sender;

static void die(const char* what) {
  fprintf(stderr, "%s: %s\n", what, strerror(errno));
  _exit(99);
}

static void on_signal(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  struct report report = {role, info->si_code, info->si_pid};
  if (write(report_pipe[1], &report, sizeof(report)) != sizeof(report)) {
    _exit(98);
  }
}

static void catch(int sig) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_signal;
  action.sa_flags = SA_SIGINFO | SA_RESTART;
  if (sigaction(sig, &action, NULL) != 0) {
    die("sigaction");
  }
}

static const char* code_name(int code) {
  switch (code) {
    case SI_USER:
      return "SI_USER";
    case SI_TKILL:
      return "SI_TKILL";
    case SI_QUEUE:
      return "SI_QUEUE";
    default:
      return "other";
  }
}

static void wait_for(int* flag, int value) {
  pthread_mutex_lock(&lock);
  while (*flag < value) {
    pthread_cond_wait(&cond, &lock);
  }
  pthread_mutex_unlock(&lock);
}

static void raise_flag(int* flag) {
  pthread_mutex_lock(&lock);
  (*flag)++;
  pthread_cond_broadcast(&cond);
  pthread_mutex_unlock(&lock);
}

/* A worker installs its mask, says it is ready, and waits. Once told to,
 * worker 2 unblocks every signal (term-queued), and `usr1_sender` sends its
 * process SIGUSR1 (usr1-self). */
/* In term-busy, worker 1 spins instead of parking (busy_worker). */
static int busy_worker;
/* The worker that unblocks every signal once told to, and the flag it raises
 * when it has (usr1-after-failed waits for it). */
static int unblock_role = 2;
static int unblocked;

static void* worker(void* arg) {
  role = (int)(long)arg;
  if (pthread_sigmask(SIG_SETMASK, &worker_masks[role], NULL) != 0) {
    die("pthread_sigmask");
  }
  raise_flag(&ready);
  if (busy_worker && role == 1) {
    /* Runnable the whole time: if the scheduler runs this thread before
     * the chosen receiver, it prints first and the test sees it. The spin
     * is long enough that the chosen thread, resumed first, exits the
     * child before it ends. */
    for (volatile unsigned long spin = 0; spin < 3000000000UL; spin++) {
    }
    printf("busy worker ran first\n");
    fflush(stdout);
  }
  wait_for(&go, 1);
  if (role == unblock_role) {
    sigset_t none;
    sigemptyset(&none);
    pthread_sigmask(SIG_SETMASK, &none, NULL);
    raise_flag(&unblocked);
  }
  if (role == usr1_sender && kill(getpid(), SIGUSR1) != 0) {
    die("kill");
  }
  for (;;) {
    pause();
  }
  return NULL;
}

/* Start the workers, worker n with worker_masks[n], and wait for them. */
static void start_workers(void) {
  for (long n = 1; n <= WORKERS; n++) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, worker, (void*)n) != 0) {
      die("pthread_create");
    }
  }
  wait_for(&ready, WORKERS);
}

static void print_report(int sender) {
  struct report report;
  ssize_t got;
  do {
    got = read(report_pipe[0], &report, sizeof(report));
  } while (got < 0 && errno == EINTR);
  if (got != sizeof(report)) {
    die("read report");
  }
  if (report.role == 0) {
    printf("receiver: main thread\n");
  } else {
    printf("receiver: worker %d\n", report.role);
  }
  printf("si_code: %s\n", code_name(report.code));
  printf("si_pid is the sender: %s\n", report.pid == sender ? "yes" : "no");
}

/* The child of the term modes. Every thread leaves SIGTERM unblocked (term),
 * every thread but worker 2 blocks it (`one`, term-one), or every thread
 * blocks it (`queued`, term-queued). */
static void term_child(int to_parent, int from_parent, int queued, int one) {
  sigset_t term, none;
  sigemptyset(&term);
  sigaddset(&term, SIGTERM);
  sigemptyset(&none);
  for (int n = 1; n <= WORKERS; n++) {
    worker_masks[n] = (queued || (one && n != 2)) ? term : none;
  }
  if (pthread_sigmask(SIG_SETMASK, (queued || one) ? &term : &none, NULL) != 0) {
    die("pthread_sigmask");
  }
  start_workers();
  char byte = 'r';
  if (write(to_parent, &byte, 1) != 1) {
    die("write ready");
  }
  if (queued) {
    if (read(from_parent, &byte, 1) != 1) {
      die("read sent");
    }
    sigset_t pending;
    sigpending(&pending);
    printf(
        "pending after the send: %s\n",
        sigismember(&pending, SIGTERM) ? "SIGTERM" : "none");
    raise_flag(&go);
  }
  print_report(getppid());
  fflush(stdout);
  _exit(0);
}

static int term(int queued, int one) {
  int to_parent[2], from_parent[2];
  if (pipe(report_pipe) != 0 || pipe(to_parent) != 0 || pipe(from_parent) != 0) {
    die("pipe");
  }
  catch(SIGTERM);
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    term_child(to_parent[1], from_parent[0], queued, one);
  }
  char byte;
  if (read(to_parent[0], &byte, 1) != 1) {
    die("read ready");
  }
  if (kill(child, SIGTERM) != 0) {
    die("kill");
  }
  if (queued && write(from_parent[1], "s", 1) != 1) {
    die("write sent");
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  printf("child exit status: %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

static int term_busy(void) {
  busy_worker = 1;
  return term(0, 0);
}

/* The child of usr1-after-failed. */
static void failed_child(int to_parent, int from_parent) {
  sigset_t usr1;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  worker_masks[1] = usr1;
  sigemptyset(&worker_masks[2]);
  worker_masks[3] = usr1;
  unblock_role = 1;
  if (pthread_sigmask(SIG_SETMASK, &usr1, NULL) != 0) {
    die("pthread_sigmask");
  }
  start_workers();
  char byte = 'r';
  if (write(to_parent, &byte, 1) != 1) {
    die("write ready");
  }
  if (read(from_parent, &byte, 1) != 1) {
    die("read failed send");
  }
  raise_flag(&go);
  wait_for(&unblocked, 1);
  if (write(to_parent, &byte, 1) != 1) {
    die("write unblocked");
  }
  print_report(getppid());
  fflush(stdout);
  _exit(0);
}

static int usr1_after_failed(void) {
  int to_parent[2], from_parent[2];
  if (pipe(report_pipe) != 0 || pipe(to_parent) != 0 || pipe(from_parent) != 0) {
    die("pipe");
  }
  catch(SIGUSR1);
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    failed_child(to_parent[1], from_parent[0]);
  }
  char byte;
  if (read(to_parent[0], &byte, 1) != 1) {
    die("read ready");
  }
  int pidfd = (int)syscall(SYS_pidfd_open, child, 0);
  if (pidfd < 0) {
    die("pidfd_open");
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = SIGUSR1;
  info.si_code = SI_USER;
  long rc = syscall(SYS_pidfd_send_signal, pidfd, SIGUSR1, &info, 0);
  printf("forged pidfd_send_signal: %ld %s\n", rc,
         rc == 0 ? "-" : errno == EPERM ? "EPERM" : "other");
  fflush(stdout);
  if (write(from_parent[1], "f", 1) != 1) {
    die("write failed send");
  }
  if (read(to_parent[0], &byte, 1) != 1) {
    die("read unblocked");
  }
  if (kill(child, SIGUSR1) != 0) {
    die("kill");
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  printf("child exit status: %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

/* The child of term-after-walk. */
static void walk_child(int to_parent) {
  sigset_t usr1, both;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  both = usr1;
  sigaddset(&both, SIGTERM);
  worker_masks[1] = usr1;
  sigemptyset(&worker_masks[2]);
  worker_masks[3] = both;
  if (pthread_sigmask(SIG_SETMASK, &both, NULL) != 0) {
    die("pthread_sigmask");
  }
  usr1_sender = 2;
  start_workers();
  raise_flag(&go);
  printf("own-process SIGUSR1:\n");
  print_report(getpid());
  char byte = 'r';
  if (write(to_parent, &byte, 1) != 1) {
    die("write ready");
  }
  printf("parent's SIGTERM:\n");
  print_report(getppid());
  fflush(stdout);
  _exit(0);
}

static int term_after_walk(void) {
  int to_parent[2];
  if (pipe(report_pipe) != 0 || pipe(to_parent) != 0) {
    die("pipe");
  }
  catch(SIGTERM);
  catch(SIGUSR1);
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    walk_child(to_parent[1]);
  }
  char byte;
  if (read(to_parent[0], &byte, 1) != 1) {
    die("read ready");
  }
  if (kill(child, SIGTERM) != 0) {
    die("kill");
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  printf("child exit status: %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  return 0;
}

/* The main thread tells the workers to go only once it is past
 * pthread_create, which blocks every signal while it runs. */
static int usr1_self(void) {
  if (pipe(report_pipe) != 0) {
    die("pipe");
  }
  catch(SIGUSR1);
  for (int n = 1; n <= WORKERS; n++) {
    sigemptyset(&worker_masks[n]);
  }
  usr1_sender = 1;
  start_workers();
  raise_flag(&go);
  print_report(getpid());
  fflush(stdout);
  _exit(0);
}

static void* send_segv(void* arg) {
  (void)arg;
  int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0);
  if (pidfd < 0) {
    die("pidfd_open");
  }
  if (syscall(SYS_pidfd_send_signal, pidfd, SIGSEGV, NULL, 0) != 0) {
    die("pidfd_send_signal");
  }
  for (;;) {
    pause();
  }
  return NULL;
}

static int segv_pidfd(void) {
  fflush(stdout);
  pid_t child = fork();
  if (child < 0) {
    die("fork");
  }
  if (child == 0) {
    /* No core dump, whatever the host's core pattern. */
    prctl(PR_SET_DUMPABLE, 0);
    pthread_t sender;
    if (pthread_create(&sender, NULL, send_segv, NULL) != 0) {
      die("pthread_create");
    }
    pthread_join(sender, NULL);
    printf("the child outlived its SIGSEGV\n");
    fflush(stdout);
    _exit(1);
  }
  int status;
  if (waitpid(child, &status, 0) != child) {
    die("waitpid");
  }
  if (WIFSIGNALED(status)) {
    printf("child killed by signal %d\n", WTERMSIG(status));
  } else {
    printf("child exit status: %d\n", WEXITSTATUS(status));
  }
  return 0;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "term") == 0) {
    return term(0, 0);
  }
  if (strcmp(mode, "term-one") == 0) {
    return term(0, 1);
  }
  if (strcmp(mode, "term-queued") == 0) {
    return term(1, 0);
  }
  if (strcmp(mode, "term-busy") == 0) {
    return term_busy();
  }
  if (strcmp(mode, "usr1-self") == 0) {
    return usr1_self();
  }
  if (strcmp(mode, "usr1-after-failed") == 0) {
    return usr1_after_failed();
  }
  if (strcmp(mode, "term-after-walk") == 0) {
    return term_after_walk();
  }
  if (strcmp(mode, "segv-pidfd") == 0) {
    return segv_pidfd();
  }
  fprintf(stderr,
          "usage: %s term|term-one|term-queued|term-busy|usr1-self|usr1-after-failed|term-after-walk|segv-pidfd\n",
          argv[0]);
  return 2;
}
