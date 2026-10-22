/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A process-directed signal (kill, rt_sigqueueinfo) to the caller's own
 * process, first with one thread and then with a second thread waiting in
 * sigsuspend() (https://github.com/rrnewton/hermit/issues/4034). Linux's
 * complete_signal picks the receiving thread from the threads that want the
 * signal; Detcore forwards the call only when the schedule fixes that
 * choice, and here the second thread sleeps in sigsuspend, outside the
 * schedule, so with two threads it refuses the call by name: a fail-closed run
 * stops with the policy-refusal status after the first block, and
 * --allow-unsupported-syscalls returns ENOSYS. A zombie child (exited, not reaped) is still signalled
 * successfully, and once reaped it is ESRCH
 * (https://github.com/rrnewton/hermit/issues/4091). A child that is never a
 * zombie is ESRCH once it exits: its parent ignores SIGCHLD or sets
 * SA_NOCLDWAIT, so Linux reaps it at exit, or it is an orphan, which the
 * container's init reaps. Linux decides at exit (a later disposition change
 * does not matter), only for a child whose exit signal is SIGCHLD, with the
 * disposition a fork inherits and an exec keeps only while it is SIG_IGN; a
 * zombie reparented to a subreaper that ignores SIGCHLD is reaped then. A
 * signal naming a live non-leader thread's id goes to its whole process, and
 * an exited thread's id names nothing.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;

static void report(const char* what, long ret);
static long queue_to(pid_t pid, int code);

/* Let the reaper Linux chose (the kernel itself, or init) finish natively. */
static void nap(void) {
  struct timespec ts = {0, 200 * 1000 * 1000};
  nanosleep(&ts, NULL);
}

/* Wait, without reaping, until the descriptor's writers have closed it. */
static void wait_eof(int fd) {
  char c;
  while (read(fd, &c, 1) > 0) {
  }
}

/* A child whose parent reaps it automatically: SIGCHLD set to SIG_IGN, or
 * SA_NOCLDWAIT. The disposition is restored afterwards. */
static void autoreaped_child(const char* what, int nocldwait) {
  struct sigaction action, saved;
  memset(&action, 0, sizeof(action));
  action.sa_handler = nocldwait ? SIG_DFL : SIG_IGN;
  action.sa_flags = nocldwait ? SA_NOCLDWAIT : 0;
  sigaction(SIGCHLD, &action, &saved);
  int done[2];
  pipe(done);
  pid_t child = fork();
  if (child == 0) {
    close(done[0]);
    _exit(0);
  }
  close(done[1]);
  wait_eof(done[0]);
  close(done[0]);
  nap();
  char label[64];
  snprintf(label, sizeof(label), "%s, kill", what);
  report(label, kill(child, SIGTERM));
  snprintf(label, sizeof(label), "%s, kill 0", what);
  report(label, kill(child, 0));
  snprintf(label, sizeof(label), "%s, rt_sigqueueinfo SI_USER", what);
  report(label, queue_to(child, SI_USER));
  sigaction(SIGCHLD, &saved, NULL);
}

static void set_sigchld(void (*handler)(int), int flags) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = handler;
  action.sa_flags = flags;
  sigaction(SIGCHLD, &action, NULL);
}

/* Fork a child that exits at once; return its pid once it has exited. */
static pid_t exited_child(void) {
  int done[2];
  pipe(done);
  pid_t child = fork();
  if (child == 0) {
    close(done[0]);
    _exit(0);
  }
  close(done[1]);
  wait_eof(done[0]);
  close(done[0]);
  nap();
  return child;
}

static void report_kill_and_0(const char* what, pid_t pid) {
  char label[96];
  snprintf(label, sizeof(label), "%s, kill", what);
  report(label, kill(pid, SIGTERM));
  snprintf(label, sizeof(label), "%s, kill 0", what);
  report(label, kill(pid, 0));
}

/* Linux decides at the child's exit whether its parent reaps it. */
static void disposition_changed_after_exit(void) {
  pid_t zombie = fork();
  if (zombie == 0) {
    _exit(0);
  }
  siginfo_t info;
  waitid(P_PID, zombie, &info, WEXITED | WNOWAIT);
  set_sigchld(SIG_IGN, 0);
  report_kill_and_0("zombie then SIG_IGN", zombie);
  set_sigchld(SIG_DFL, 0);
  waitpid(zombie, NULL, 0);

  set_sigchld(SIG_IGN, 0);
  pid_t gone = exited_child();
  set_sigchld(SIG_DFL, 0);
  report_kill_and_0("auto-reaped then SIG_DFL", gone);
}

/* A forked child inherits SIG_IGN, so its own child is reaped at exit. */
static void inherited_sig_ign(void) {
  set_sigchld(SIG_IGN, 0);
  int done[2];
  pipe(done);
  pid_t middle = fork();
  if (middle == 0) {
    close(done[0]);
    report_kill_and_0("inherited SIG_IGN child", exited_child());
    _exit(0);
  }
  close(done[1]);
  wait_eof(done[0]);
  close(done[0]);
  set_sigchld(SIG_DFL, 0);
}

static int clone_exit(void* unused) {
  (void)unused;
  return 0;
}

/* A clone child whose exit signal is not SIGCHLD is never reaped
 * automatically, whatever its parent's SIGCHLD disposition. */
static void clone_exit_signal_0(void) {
  static char stack[64 * 1024];
  set_sigchld(SIG_IGN, 0);
  pid_t child = clone(clone_exit, stack + sizeof(stack), 0, NULL);
  siginfo_t info;
  waitid(P_PID, child, &info, WEXITED | WNOWAIT | __WALL);
  report_kill_and_0("exit signal 0, SIG_IGN", child);
  waitpid(child, NULL, __WALL);
  set_sigchld(SIG_DFL, 0);
}

/* After exec, SIG_IGN still reaps a child at exit; SA_NOCLDWAIT on the default
 * action is cleared, so the child stays a zombie. Runs in the exec'd image. */
static int exec_child_mode(const char* label) {
  report_kill_and_0(label, exited_child());
  return 0;
}

static void after_exec(const char* self, int nocldwait) {
  if (nocldwait) {
    set_sigchld(SIG_DFL, SA_NOCLDWAIT);
  } else {
    set_sigchld(SIG_IGN, 0);
  }
  int done[2];
  pipe(done);
  pid_t middle = fork();
  if (middle == 0) {
    close(done[0]);
    char* args[] = {(char*)self, nocldwait ? "exec-nocldwait" : "exec-ign", NULL};
    execv(self, args);
    _exit(99);
  }
  close(done[1]);
  wait_eof(done[0]);
  close(done[0]);
  set_sigchld(SIG_DFL, 0);
}

/* A zombie whose parent exits goes to the subreaper; one that ignores
 * SIGCHLD reaps it at once. Sets the subreaper flag, so it runs last. */
static void zombie_reparented_to_ignoring_subreaper(void) {
  prctl(PR_SET_CHILD_SUBREAPER, 1);
  int pid_pipe[2], ignoring[2];
  pipe(pid_pipe);
  pipe(ignoring);
  pid_t middle = fork();
  if (middle == 0) {
    close(pid_pipe[0]);
    close(ignoring[1]);
    pid_t grandchild = fork();
    if (grandchild == 0) {
      _exit(0);
    }
    siginfo_t info;
    waitid(P_PID, grandchild, &info, WEXITED | WNOWAIT);
    write(pid_pipe[1], &grandchild, sizeof(grandchild));
    /* Exit, orphaning the zombie, only once the subreaper ignores SIGCHLD. */
    wait_eof(ignoring[0]);
    _exit(0);
  }
  close(pid_pipe[1]);
  close(ignoring[0]);
  pid_t grandchild;
  read(pid_pipe[0], &grandchild, sizeof(grandchild));
  set_sigchld(SIG_IGN, 0);
  close(ignoring[1]);
  wait_eof(pid_pipe[0]);
  close(pid_pipe[0]);
  nap();
  report_kill_and_0("zombie reparented to a SIG_IGN subreaper", grandchild);
  set_sigchld(SIG_DFL, 0);
}

/* A grandchild orphaned by its parent's exit, with no subreaper: the
 * container's init adopts and reaps it. */
static void orphaned_grandchild(void) {
  int pid_pipe[2], parent_gone[2];
  pipe(pid_pipe);
  pipe(parent_gone);
  pid_t child = fork();
  if (child == 0) {
    close(pid_pipe[0]);
    close(parent_gone[1]);
    pid_t grandchild = fork();
    if (grandchild == 0) {
      wait_eof(parent_gone[0]);
      close(pid_pipe[1]);
      _exit(0);
    }
    write(pid_pipe[1], &grandchild, sizeof(grandchild));
    close(pid_pipe[1]);
    _exit(0);
  }
  close(pid_pipe[1]);
  close(parent_gone[0]);
  pid_t grandchild;
  read(pid_pipe[0], &grandchild, sizeof(grandchild));
  waitpid(child, NULL, 0);
  close(parent_gone[1]);
  wait_eof(pid_pipe[0]);
  close(pid_pipe[0]);
  nap();
  report("orphan, kill", kill(grandchild, SIGTERM));
  report("orphan, kill 0", kill(grandchild, 0));
}

static void on_usr1(int sig) {
  (void)sig;
  handled++;
}

static void report(const char* what, long ret) {
  if (ret == 0) {
    printf("%s: 0\n", what);
  } else {
    printf("%s: %ld %s\n", what, ret, strerrorname_np(errno));
  }
}

static long queue_to_self(void) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = SIGUSR1;
  info.si_code = SI_QUEUE;
  info.si_pid = getpid();
  info.si_uid = getuid();
  return syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, &info);
}

/* rt_sigqueueinfo to another process; SI_USER from a process other than the
 * target is a forged code, which Linux refuses with EPERM. */
static long queue_sig_to(pid_t pid, int sig, int code, int padded) {
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  info.si_signo = sig;
  info.si_code = code;
  info.si_pid = getpid();
  info.si_uid = getuid();
  /* A nonzero byte in the padding: an unknown code with it is E2BIG. */
  if (padded) {
    ((unsigned char*)&info)[100] = 0xff;
  }
  return syscall(SYS_rt_sigqueueinfo, pid, sig, &info);
}

static long queue_to(pid_t pid, int code) {
  return queue_sig_to(pid, SIGTERM, code, 0);
}

static volatile int stop;

static void* record_tid(void* tid) {
  *(pid_t*)tid = gettid();
  return NULL;
}

/* The second thread waits with SIGUSR1 blocked except inside sigsuspend, so
 * the final pthread_kill cannot slip in between its check and its wait. */
static int pauser_tid_pipe[2];

static void* pauser(void* unused) {
  (void)unused;
  pid_t tid = gettid();
  write(pauser_tid_pipe[1], &tid, sizeof(tid));
  sigset_t usr1, none;
  sigemptyset(&usr1);
  sigaddset(&usr1, SIGUSR1);
  sigemptyset(&none);
  pthread_sigmask(SIG_BLOCK, &usr1, NULL);
  while (!stop) {
    sigsuspend(&none);
  }
  return NULL;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 1 && strcmp(argv[1], "exec-ign") == 0) {
    return exec_child_mode("after exec, SIG_IGN");
  }
  if (argc > 1 && strcmp(argv[1], "exec-nocldwait") == 0) {
    return exec_child_mode("after exec, SA_NOCLDWAIT");
  }
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);

  report("one thread, kill", kill(getpid(), SIGUSR1));
  report("one thread, rt_sigqueueinfo", queue_to_self());
  printf("one thread, handled: %d\n", (int)handled);

  /* The id of a thread that has exited names nothing. */
  pthread_t exited;
  pid_t exited_tid = 0;
  pthread_create(&exited, NULL, record_tid, &exited_tid);
  pthread_join(exited, NULL);
  report("exited thread id, kill", kill(exited_tid, SIGUSR1));
  report("exited thread id, kill 0", kill(exited_tid, 0));

  pid_t zombie = fork();
  if (zombie == 0) {
    _exit(0);
  }
  siginfo_t info;
  waitid(P_PID, zombie, &info, WEXITED | WNOWAIT);
  report("zombie, kill", kill(zombie, SIGTERM));
  report("zombie, kill 0", kill(zombie, 0));
  report("zombie, kill 99", kill(zombie, 99));
  report("zombie, rt_sigqueueinfo", queue_to(zombie, SI_QUEUE));
  report("zombie, rt_sigqueueinfo SI_USER", queue_to(zombie, SI_USER));
  waitpid(zombie, NULL, 0);
  report("reaped, kill", kill(zombie, SIGTERM));
  report("reaped, kill 0", kill(zombie, 0));
  report("reaped, rt_sigqueueinfo", queue_to(zombie, SI_QUEUE));
  /* Linux checks the siginfo before it looks a gone pid up
   * (https://github.com/rrnewton/hermit/issues/4124). */
  report("reaped, rt_sigqueueinfo SI_USER", queue_to(zombie, SI_USER));
  report("reaped, rt_sigqueueinfo unknown code with padding", queue_sig_to(zombie, SIGTERM, -100, 1));
  report("reaped, rt_sigqueueinfo bad pointer", syscall(SYS_rt_sigqueueinfo, zombie, SIGTERM, (void*)8));
  report("reaped, rt_sigqueueinfo 0", queue_sig_to(zombie, 0, SI_QUEUE, 0));

  autoreaped_child("SIG_IGN child", 0);
  autoreaped_child("SA_NOCLDWAIT child", 1);
  orphaned_grandchild();
  disposition_changed_after_exit();
  inherited_sig_ign();
  clone_exit_signal_0();
  after_exec(argv[0], 0);
  after_exec(argv[0], 1);
  zombie_reparented_to_ignoring_subreaper();

  pthread_t thread;
  pipe(pauser_tid_pipe);
  pthread_create(&thread, NULL, pauser, NULL);
  pid_t pauser_tid;
  read(pauser_tid_pipe[0], &pauser_tid, sizeof(pauser_tid));
  report("two threads, kill", kill(getpid(), SIGUSR1));
  report("two threads, rt_sigqueueinfo", queue_to_self());
  /* A live non-leader thread's id names its whole process
   * (https://github.com/rrnewton/hermit/issues/4094). */
  report("two threads, kill by thread id", kill(pauser_tid, SIGUSR1));
  /* With two live threads, an exited thread's id still names nothing. */
  report("two threads, kill by exited thread id", kill(exited_tid, SIGUSR1));

  stop = 1;
  pthread_kill(thread, SIGUSR1);
  pthread_join(thread, NULL);
  return 0;
}
