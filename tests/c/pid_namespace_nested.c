/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* Guest for hermit-cli/tests/cli.rs: pids inside a nested PID namespace
 * (https://github.com/rrnewton/hermit/issues/4155,
 * https://github.com/rrnewton/hermit/issues/4090).
 *
 * usage: pid_namespace_nested <mode>
 *
 * The outer process clones a nested PID namespace's init with
 * clone(CLONE_NEWPID); the init runs one mode, prints one line, and the outer
 * process prints the init's exit status. Natively it needs
 * `unshare --user --map-root-user`.
 *   self_kill:    the init sends kill(getpid(), SIGUSR1) with a handler: rc 0,
 *                 and the handler ran.
 *   fork_wait:    the init forks; fork's return, the child's getpid and
 *                 getppid, and waitpid(child) with its status.
 *   wait_any:     the init forks and waits with waitpid(-1).
 *   sibling:      the init forks two children (pids 2 and 3) and sends
 *                 SIGUSR1 to pid 3, whose handler exits 7. Under Hermit the
 *                 outer process's own pid is 3 in the container's namespace,
 *                 so a pid read as a container pid signals the wrong process.
 *   init_sigkill: the init sends kill(1, SIGKILL) and kill(getpid(), SIGTERM)
 *                 with default dispositions: Linux drops both and answers 0.
 *   orphan:       the init forks a child that forks a grandchild and exits;
 *                 the grandchild's getppid() becomes 1, the nested init, which
 *                 reaps it with waitpid(-1).
 *   outer:        the init sends kill(<the outer process's pid>, 0): no task of
 *                 its namespace has that pid, so ESRCH.
 *   tgkill:       the init starts a thread and sends tgkill(getpid(), <its
 *                 tid>, SIGUSR1), which runs the handler on that thread.
 *   init_leaves_zombie: the init forks a child that exits, and exits without
 *                 waiting for it; only a zombie is left in the namespace.
 *   reap_then_kill: the init forks a child and reaps it, then sends it
 *                 kill(pid, 0) and kill(pid, SIGUSR1): ESRCH for both, since
 *                 a reaped pid names nothing.
 *   outer_zombie: before cloning, the outer process leaves a child as an
 *                 unreaped zombie; the init names that child's outer pid, which
 *                 no task of its namespace has: kill(pid, 0) and
 *                 kill(pid, SIGUSR1) are ESRCH, and waitpid(pid, WNOHANG) is
 *                 ECHILD.
 *   pidfd_selfkill: the init opens a pidfd to itself (pid 1) and sends
 *                 SIGKILL through it: dropped, rc 0, the init stays alive.
 *   pidfd_getfd:  the init duplicates its own pipe's read end with
 *                 pidfd_getfd on a pidfd to itself and reads the byte back.
 *   pgid_wait:    the init makes itself a group leader with setpgid(1, 1)
 *                 and exits 3; the outer process (depth 0) reaps it with
 *                 waitpid(-<init's pid>), a wait on that group.
 *   pgid_sigkill: the init puts a child in its own group with setpgid(k, k);
 *                 the outer process finds the child's pid (the process whose
 *                 /proc status names the init as PPid) and sends kill(-pid,
 *                 SIGKILL) to that group; the init reaps the child, killed.
 *   cohort_sigchld: the init blocks SIGCHLD and puts two children in one
 *                 group; a depth-0 helper joins that group and SIGKILLs it,
 *                 itself included; the init takes the SIGCHLDs with
 *                 sigtimedwait. Linux coalesces them into one or delivers
 *                 two, each naming a child with CLD_KILLED and status 9.
 *                 Under Hermit the second is the scheduler's own copy.
 *   cohort_signalfd: the same, read from a signalfd; each record's ssi_int
 *                 and ssi_ptr must be 0, as in Linux's child-exit record.
 *   setpgid:      the init forks a child and puts it in its own group with
 *                 setpgid(child, child); the child sees pgid 2.
 *   prlimit:      the init reads its own RLIMIT_NOFILE with prlimit64(1, ...).
 *   sched_setattr: the init sets SCHED_OTHER on its thread by the thread's
 *                 nested tid.
 *   init_dies:    the init forks a child that waits forever, and exits; Linux
 *                 kills the child (zap_pid_namespace) at a moment the host
 *                 chooses, which Hermit reports as a determinism loss.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <linux/sched/types.h>
#include <dirent.h>
#include <linux/futex.h>
#include <sys/mman.h>
#include <poll.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t ran, ran_tid;
static void on_usr1(int sig) {
  (void)sig;
  ran = 1;
  ran_tid = (int)syscall(SYS_gettid);
}
static void on_usr1_exit(int sig) {
  (void)sig;
  _exit(7);
}
static void handle(int sig, void (*fn)(int)) {
  struct sigaction a;
  memset(&a, 0, sizeof a);
  a.sa_handler = fn;
  if (sigaction(sig, &a, NULL)) _exit(90);
}
static const char *err(int rc) { return rc ? strerrorname_np(errno) : "0"; }

static void *idle(void *arg) {
  volatile int *tid = arg;
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  pthread_sigmask(SIG_UNBLOCK, &set, NULL);
  *tid = (int)syscall(SYS_gettid);
  while (!ran) {
    struct timespec ms = {0, 1000000};
    nanosleep(&ms, NULL);
  }
  return NULL;
}

static int outer_pid, outer_zombie_pid;

/* The pid of a process whose parent is `parent`, from /proc, or 0. */
static int child_of(int parent) {
  DIR *d = opendir("/proc");
  if (!d) return 0;
  struct dirent *e;
  int found = 0;
  while (!found && (e = readdir(d))) {
    int pid = atoi(e->d_name);
    if (pid <= 0) continue;
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/%d/status", pid);
    FILE *f = fopen(path, "r");
    if (!f) continue;
    while (fgets(line, sizeof line, f)) {
      if (!strncmp(line, "PPid:", 5)) {
        if (atoi(line + 5) == parent) found = pid;
        break;
      }
    }
    fclose(f);
  }
  closedir(d);
  return found;
}
static int sync_pipe[2], go_pipe[2];
static volatile int *done_word;

static int init_main(const char *mode) {
  if (!strcmp(mode, "self_kill")) {
    handle(SIGUSR1, on_usr1);
    int rc = kill(getpid(), SIGUSR1);
    printf("self_kill pid=%d rc=%s ran=%d\n", getpid(), err(rc), ran);
  } else if (!strcmp(mode, "fork_wait")) {
    pid_t c = fork();
    if (c == 0) {
      printf("child pid=%d ppid=%d\n", getpid(), getppid());
      fflush(stdout);
      _exit(5);
    }
    int st;
    pid_t w = waitpid(c, &st, 0);
    printf("fork_wait fork=%d waitpid=%d status=%d\n", c, w, WEXITSTATUS(st));
  } else if (!strcmp(mode, "wait_any")) {
    pid_t c = fork();
    if (c == 0) _exit(6);
    int st;
    pid_t w = waitpid(-1, &st, 0);
    printf("wait_any fork=%d waitpid=%d status=%d\n", c, w, WEXITSTATUS(st));
  } else if (!strcmp(mode, "sibling")) {
    int ready[2];
    if (pipe(ready)) _exit(94);
    pid_t a = fork();
    if (a == 0) {
      for (;;) pause();
    }
    pid_t b = fork();
    if (b == 0) {
      handle(SIGUSR1, on_usr1_exit);
      char byte = 1;
      if (write(ready[1], &byte, 1) != 1) _exit(95);
      for (;;) pause();
    }
    char byte;
    if (read(ready[0], &byte, 1) != 1) _exit(96);
    int rc = kill(3, SIGUSR1);
    int st;
    pid_t w = waitpid(b, &st, 0);
    kill(a, SIGKILL);
    waitpid(a, NULL, 0);
    printf("sibling a=%d b=%d kill3=%s waited=%d status=%d\n", a, b, err(rc), w,
           WIFEXITED(st) ? WEXITSTATUS(st) : -1);
  } else if (!strcmp(mode, "init_sigkill")) {
    int rc1 = kill(1, SIGKILL);
    int rc2 = kill(getpid(), SIGTERM);
    printf("init_sigkill kill1=%s term=%s alive=1\n", err(rc1), err(rc2));
  } else if (!strcmp(mode, "orphan")) {
    int fds[2];
    if (pipe(fds)) _exit(91);
    pid_t c = fork();
    if (c == 0) {
      pid_t g = fork();
      if (g == 0) {
        close(fds[0]);
        while (getppid() != 1) usleep(1000);
        int ppid = getppid();
        if (write(fds[1], &ppid, sizeof ppid) != sizeof ppid) _exit(92);
        _exit(0);
      }
      _exit(0);
    }
    close(fds[1]);
    waitpid(c, NULL, 0);
    int ppid = -1;
    if (read(fds[0], &ppid, sizeof ppid) != sizeof ppid) ppid = -2;
    int st;
    pid_t w = waitpid(-1, &st, 0);
    printf("orphan grandchild_ppid=%d reaped=%d\n", ppid, w);
  } else if (!strcmp(mode, "outer")) {
    int rc = kill(outer_pid, 0);
    printf("outer kill0=%s\n", err(rc));
  } else if (!strcmp(mode, "tgkill")) {
    handle(SIGUSR1, on_usr1);
    volatile int tid = 0;
    pthread_t t;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &set, NULL);
    if (pthread_create(&t, NULL, idle, (void *)&tid)) _exit(93);
    while (!tid) usleep(1000);
    int rc = (int)syscall(SYS_tgkill, getpid(), tid, SIGUSR1);
    pthread_join(t, NULL);
    printf("tgkill thread=%d rc=%s on_thread=%d\n", tid, err(rc), ran_tid == tid);
  } else if (!strcmp(mode, "reap_then_kill")) {
    pid_t c = fork();
    if (c == 0) _exit(0);
    waitpid(c, NULL, 0);
    int rc0 = kill(c, 0);
    const char *e0 = err(rc0);
    int rc1 = kill(c, SIGUSR1);
    printf("reap_then_kill child=%d kill0=%s kill_usr1=%s\n", c, e0, err(rc1));
  } else if (!strcmp(mode, "outer_zombie")) {
    pid_t z = outer_zombie_pid;
    int rc0 = kill(z, 0);
    const char *e0 = err(rc0);
    int rc1 = kill(z, SIGUSR1);
    const char *e1 = err(rc1);
    int w = waitpid(z, NULL, WNOHANG);
    printf("outer_zombie kill0=%s kill_usr1=%s waitpid=%s\n", e0, e1, err(w));
  } else if (!strcmp(mode, "pidfd_selfkill")) {
    int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0U);
    if (pidfd < 0) _exit(102);
    errno = 0;
    long rc = syscall(SYS_pidfd_send_signal, pidfd, SIGKILL, NULL, 0U);
    printf("pidfd_selfkill rc=%s alive=1\n", err((int)rc));
  } else if (!strcmp(mode, "pidfd_getfd")) {
    int pidfd = (int)syscall(SYS_pidfd_open, getpid(), 0U);
    int fds[2];
    if (pidfd < 0 || pipe(fds) || write(fds[1], "Q", 1) != 1) _exit(103);
    int copy = (int)syscall(SYS_pidfd_getfd, pidfd, fds[0], 0U);
    if (copy < 0) {
      printf("pidfd_getfd errno=%s\n", strerrorname_np(errno));
    } else {
      char byte = 0;
      ssize_t n = read(copy, &byte, 1);
      printf("pidfd_getfd n=%zd byte=%c\n", n, byte);
    }
  } else if (!strcmp(mode, "pgid_wait")) {
    int rc = setpgid(1, 1);
    printf("pgid_wait init setpgid=%s\n", err(rc));
    fflush(stdout);
    if (write(sync_pipe[1], "x", 1) != 1) _exit(104);
    return 3;
  } else if (!strcmp(mode, "pgid_sigkill")) {
    pid_t k = fork();
    if (k == 0) {
      for (;;) pause();
    }
    int rc = setpgid(k, k);
    if (write(sync_pipe[1], "x", 1) != 1) _exit(105);
    char b;
    if (read(go_pipe[0], &b, 1) != 1) _exit(106);
    int st;
    pid_t w = waitpid(k, &st, 0);
    printf("pgid_sigkill init setpgid=%s waited=%d sig=%d\n", err(rc), w == k,
           WIFSIGNALED(st) ? WTERMSIG(st) : -1);
  } else if (!strcmp(mode, "forged_mark")) {
    /* A SIGCHLD carrying the scheduler's own-copy mark (SI_QUEUE with the
       value 0x48434c44) is refused by name under Hermit. */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGCHLD);
    sigprocmask(SIG_BLOCK, &set, NULL);
    siginfo_t info;
    memset(&info, 0, sizeof info);
    info.si_signo = SIGCHLD;
    info.si_code = SI_QUEUE;
    info.si_value.sival_int = 0x48434c44;
    long rc = syscall(SYS_rt_sigqueueinfo, getpid(), SIGCHLD, &info);
    printf("forged_mark rc=%s\n", err(rc));
  } else if (!strcmp(mode, "cohort_sigchld") || !strcmp(mode, "cohort_signalfd")) {
    /* cohort_signalfd reads the SIGCHLDs from a signalfd, and also prints
       ssi_int and ssi_ptr, which Linux leaves 0 in a child-exit record. */
    int use_sfd = !strcmp(mode, "cohort_signalfd");
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGCHLD);
    sigprocmask(SIG_BLOCK, &set, NULL);
    pid_t k = fork();
    if (k == 0) {
      for (;;) pause();
    }
    pid_t j = fork();
    if (j == 0) {
      for (;;) pause();
    }
    int rk = setpgid(k, k), rj = setpgid(j, k);
    if (write(sync_pipe[1], "x", 1) != 1) _exit(107);
    int n = 0, codes[3], pids[3], statuses[3];
    unsigned long long extras[3] = {0, 0, 0};
    int sfd = use_sfd ? signalfd(-1, &set, SFD_NONBLOCK) : -1;
    if (use_sfd && sfd < 0) _exit(110);
    for (; n < 3; n++) {
      if (use_sfd) {
        struct pollfd ready = {.fd = sfd, .events = POLLIN};
        if (poll(&ready, 1, n == 0 ? 2000 : 50) <= 0) break;
        struct signalfd_siginfo record;
        if (read(sfd, &record, sizeof record) != (ssize_t)sizeof record ||
            record.ssi_signo != SIGCHLD)
          break;
        codes[n] = record.ssi_code;
        pids[n] = (int)record.ssi_pid;
        statuses[n] = record.ssi_status;
        extras[n] = (unsigned long long)record.ssi_int | record.ssi_ptr;
        continue;
      }
      siginfo_t info;
      memset(&info, 0, sizeof info);
      struct timespec wait = {n == 0 ? 2 : 0, n == 0 ? 0 : 50000000};
      if (sigtimedwait(&set, &info, &wait) != SIGCHLD) break;
      codes[n] = info.si_code;
      pids[n] = info.si_pid;
      statuses[n] = info.si_status;
    }
    /* The helper, a zombie in the group, pins its pgid, the pid of the
       init's child k: the init must not exit until the helper is reaped
       (zap_pid_namespace would wait for that pid forever). */
    *done_word = 1;
    syscall(SYS_futex, done_word, FUTEX_WAKE, 1, NULL, NULL, 0);
    char b;
    if (read(go_pipe[0], &b, 1) != 1) _exit(109);
    int sk = 0, sj = 0;
    waitpid(k, &sk, 0);
    waitpid(j, &sj, 0);
    int genuine = n >= 1 && n <= 2;
    for (int i = 0; i < n; i++) {
      if (use_sfd)
        printf("%s init took code=%d pid=%d status=%d int_ptr=%#llx\n", mode, codes[i], pids[i],
               statuses[i], extras[i]);
      else
        printf("%s init took code=%d pid=%d status=%d\n", mode, codes[i], pids[i], statuses[i]);
      if ((pids[i] != k && pids[i] != j) || codes[i] != CLD_KILLED || statuses[i] != SIGKILL ||
          extras[i] != 0)
        genuine = 0;
    }
    printf("%s init setpgid=%s,%s killed=%d,%d genuine=%d\n", mode, err(rk), err(rj),
           WIFSIGNALED(sk) && WTERMSIG(sk) == SIGKILL, WIFSIGNALED(sj) && WTERMSIG(sj) == SIGKILL,
           genuine);
  } else if (!strcmp(mode, "setpgid")) {
    int ready[2];
    if (pipe(ready)) _exit(98);
    pid_t c = fork();
    if (c == 0) {
      char b;
      if (read(ready[0], &b, 1) != 1) _exit(99);
      printf("child pid=%d pgid=%d\n", getpid(), getpgid(0));
      fflush(stdout);
      _exit(0);
    }
    int rc = setpgid(c, c);
    int group = getpgid(c);
    if (write(ready[1], "x", 1) != 1) _exit(100);
    waitpid(c, NULL, 0);
    printf("setpgid rc=%s getpgid=%d\n", err(rc), group);
  } else if (!strcmp(mode, "prlimit")) {
    struct rlimit old;
    int rc = (int)syscall(SYS_prlimit64, getpid(), RLIMIT_NOFILE, NULL, &old);
    printf("prlimit pid=%d rc=%s\n", getpid(), err(rc));
  } else if (!strcmp(mode, "sched_setattr")) {
    volatile int tid = 0;
    pthread_t t;
    if (pthread_create(&t, NULL, idle, (void *)&tid)) _exit(101);
    while (!tid) usleep(1000);
    struct sched_attr attr;
    memset(&attr, 0, sizeof attr);
    attr.size = sizeof attr;
    attr.sched_policy = SCHED_OTHER;
    int rc = (int)syscall(SYS_sched_setattr, tid, &attr, 0);
    ran = 1;
    pthread_join(t, NULL);
    printf("sched_setattr thread=%d rc=%s\n", tid, err(rc));
  } else if (!strcmp(mode, "init_leaves_zombie")) {
    int fds[2];
    if (pipe(fds)) _exit(97);
    pid_t c = fork();
    if (c == 0) _exit(0);
    /* Wait until the child has exited, without reaping it. */
    siginfo_t info;
    memset(&info, 0, sizeof info);
    waitid(P_PID, c, &info, WEXITED | WNOWAIT);
    printf("init_leaves_zombie child=%d\n", c);
  } else if (!strcmp(mode, "init_dies")) {
    pid_t c = fork();
    if (c == 0) {
      for (;;) pause();
    }
    printf("init_dies child=%d\n", c);
  } else {
    return 64;
  }
  fflush(stdout);
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "self_kill";
  outer_pid = getpid();
  if (pipe(sync_pipe) || pipe(go_pipe)) return 82;
  done_word = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (done_word == MAP_FAILED) return 82;
  if (!strcmp(mode, "outer_zombie")) {
    outer_zombie_pid = fork();
    if (outer_zombie_pid == 0) _exit(0);
    siginfo_t info;
    memset(&info, 0, sizeof info);
    waitid(P_PID, outer_zombie_pid, &info, WEXITED | WNOWAIT);
  }
  fflush(stdout);
  long c = syscall(SYS_clone, (long)(CLONE_NEWPID | SIGCHLD), 0L, 0L, 0L, 0L);
  if (c < 0) {
    printf("clone(CLONE_NEWPID) %s\n", strerrorname_np(errno));
    return 0;
  }
  if (c == 0) _exit(init_main(mode));
  int st;
  if (!strcmp(mode, "pgid_wait")) {
    char b;
    if (read(sync_pipe[0], &b, 1) != 1) return 83;
    pid_t w = waitpid(-(pid_t)c, &st, 0);
    printf("pgid_wait outer waitpid=%s reaped_init=%d status=%d\n", err(w), w == (pid_t)c,
           w > 0 && WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
  }
  if (!strcmp(mode, "cohort_sigchld") || !strcmp(mode, "cohort_signalfd")) {
    /* A depth-0 helper joins the nested group and SIGKILLs it, itself
       included, so no sender is left to take a turn after the send. */
    pid_t x = fork();
    if (x == 0) {
      char b;
      if (read(sync_pipe[0], &b, 1) != 1) _exit(90);
      int k = child_of((int)c);
      int g = k > 0 ? getpgid(k) : -1;
      if (g <= 0 || setpgid(0, g) != 0) {
        kill((pid_t)c, SIGKILL);
        _exit(91);
      }
      kill(-g, SIGKILL);
      _exit(92);
    }
    /* A futex wait parks the outer process: it takes no turn until the
       init wakes it. */
    while (!*done_word) syscall(SYS_futex, done_word, FUTEX_WAIT, 0, NULL, NULL, 0);
    int sx = 0;
    waitpid(x, &sx, 0);
    if (write(go_pipe[1], "x", 1) != 1) return 90;
    if (waitpid((pid_t)c, &st, 0) != (pid_t)c) return 88;
    printf("%s helper sig=%d\n", mode, WIFSIGNALED(sx) ? WTERMSIG(sx) : -1);
    printf("init exit=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    return 0;
  }
  if (!strcmp(mode, "pgid_sigkill")) {
    char b;
    if (read(sync_pipe[0], &b, 1) != 1) return 84;
    int k = child_of((int)c);
    int rc = k > 0 ? kill(-k, SIGKILL) : -2;
    printf("pgid_sigkill outer found=%d kill=%s\n", k > 0, rc == -2 ? "none" : err(rc));
    fflush(stdout);
    /* Without k the init would wait for it forever: end the namespace. */
    if (k <= 0) kill((pid_t)c, SIGKILL);
    if (write(go_pipe[1], "x", 1) != 1) return 85;
  }
  if (waitpid((pid_t)c, &st, 0) != (pid_t)c) return 81;
  if (outer_zombie_pid > 0) waitpid(outer_zombie_pid, NULL, 0);
  printf("init exit=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
  return 0;
}
