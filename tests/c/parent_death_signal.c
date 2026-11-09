/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A parent-death signal armed with prctl(PR_SET_PDEATHSIG)
// (https://github.com/rrnewton/hermit/issues/4440). Modes:
//   term      a second thread forks the child, which arms SIGTERM and
//             pauses; that thread exits (its process lives on), so Linux
//             sends the child SIGTERM: "term: child signal 15"
//   caught    the same with a caught SIGUSR1: the child's pause returns
//             after the handler ran, and it exits 7: "caught: child exit 7"
//   kill      the same with SIGKILL, which no handler or mask stops:
//             "kill: child signal 9"
//   get       PR_GET_PDEATHSIG reads back what PR_SET_PDEATHSIG armed, and a
//             forked child starts with none: "get: 10, child 0"
//   subreaper the main process is a child subreaper; a middle process forks
//             the child, which arms SIGTERM and pauses, then exits: the child
//             gets SIGTERM and is adopted: "subreaper: child signal 15"
//   group     the same, but the middle process's worker thread forks the
//             child and the main thread calls exit_group while the worker
//             lives: the worker's exit sends it: "group: child signal 15"
//   exec      a second thread forks the child, which arms a caught SIGUSR1
//             (exit 3 if it comes) and sleeps a second, then the thread
//             execs: it survives as the leader, so nothing is sent and the
//             new image reaps "exec: child exit 0"
//   invalid   the child arms SIGTERM, then tries 99, which Linux refuses
//             with EINVAL leaving SIGTERM armed: "invalid: get 15 einval 1",
//             then "invalid: child signal 15"
//   stop      arms SIGSTOP, reads it back and disarms it, so it never fires:
//             Linux accepts every signal from 1 to 64,
//             "stop: rc 0 errno 0 armed 1"
//   realtime  the same with SIGRTMIN: "realtime: rc 0 errno 0 armed 1"
//   sigchld   the same with SIGCHLD: "sigchld: rc 0 errno 0 armed 1"
//   nothread  as caught, but the main thread reaps the child with
//             __WNOTHREAD: when the creator thread exits, the child becomes
//             the main thread's (find_alive_thread): "nothread: child exit 7"
//   setid     each credential setter in a fresh child that armed SIGUSR1: one
//             that moves an effective or filesystem id clears the signal
//             (commit_creds), the others keep it; needs root, as Hermit's
//             virtual root is: "setid: setuid 0 setgid 0 setreuid 0 setregid
//             0 setresuid 0 setresgid 0 setfsuid 0 setfsgid 0 ruid 10
//             setuid0 10 groups 10"
//   secure-exec IMAGE  arms SIGUSR1 and execs IMAGE (a copy of this program)
//             as "after-secure-exec", which prints what it holds and its
//             AT_SECURE: a set-id IMAGE that changes an effective id runs in
//             secure-execution mode, which clears the signal:
//             "secure-exec: armed 0 at_secure 1"
//   zombieleader  the main process is a child subreaper; a middle process
//             forks the child, whose leader arms a caught SIGUSR1, starts a
//             worker and leaves with pthread_exit; once it has gone, the
//             worker tells the middle process, which exits, and pauses until
//             the handler has run, then exits 7. Linux keeps the leader on
//             the thread list as a zombie, so it still sends the leader's
//             signal: "zombieleader: child exit 7"
//   resend    the main process and an inner process below it are child
//             subreapers; a middle process below that forks the child, which
//             arms SIGUSR1 and counts each one it handles. The middle
//             process exits: the child gets the signal and goes to the inner
//             subreaper. Once it has handled it, the inner one exits too, and
//             the child gets it again: each exit of its parent sends it. The
//             child exits with the count: "resend: child exit 2"
//   threadresend  as resend, but the child's first parent is a second thread
//             of the middle process: that thread's exit sends the signal and
//             makes the middle process's main thread the child's parent
//             (find_alive_thread); once the child has handled it, the main
//             thread's exit sends it again: "threadresend: child exit 2"
//   futex     as subreaper, but the child waits on a futex word that nothing
//             wakes: the signal ends the wait and the child,
//             "futex: child signal 15"
//   capexec   drops a permitted capability the bounding set still holds,
//             arms SIGUSR1 and execs this program as "after-capexec capexec
//             BIT", which prints whether the capability came back, what it
//             holds and its AT_SECURE: as root the exec gives the capability
//             back, so the permitted set grows and commit_creds clears the
//             signal although the exec is not secure,
//             "capexec: regained 1 armed 0 at_secure 0"; with no_new_privs
//             the exec gives nothing back and the signal stays,
//             "capexec: regained 0 armed 10 at_secure 0"
//   capexecall  the same, but it arms first, then empties every capability
//             set and execs /proc/self/exe:
//             "capexecall: regained 1 armed 0 at_secure 0", or with
//             no_new_privs "capexecall: regained 0 armed 10 at_secure 0"
//   tightexec arms SIGUSR1 and execs this program as "after-tightexec" with
//             its stack pointer 64 bytes above a guard page, so no scratch is
//             free below the red zone; the new image prints what it holds:
//             "tightexec: armed 10"
//   tightcap  capexec's drop and arm, then tightexec's exec from a stack 64
//             bytes above a guard page: as root the exec gives the capability
//             back and commit_creds clears the signal,
//             "tightcap: regained 1 armed 0 at_secure 0"
#define _GNU_SOURCE
#include <errno.h>
#include <linux/capability.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/fsuid.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <grp.h>
#include <unistd.h>

static pid_t child;
static int ready[2];
static volatile sig_atomic_t handled;

static void on_usr1(int sig) {
  (void)sig;
  handled = 1;
}

/* The child's body: arm `signal`, report readiness, and pause until a
 * handler has run (or the signal's default action ends it). */
static void armed_child(int signal) {
  if (signal == SIGUSR1) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_usr1;
    sigaction(SIGUSR1, &action, NULL);
  }
  prctl(PR_SET_PDEATHSIG, signal);
  char byte = 'x';
  if (write(ready[1], &byte, 1) != 1) {
    _exit(9);
  }
  while (!handled) {
    pause();
  }
  _exit(7);
}

/* The futex mode's child: arm SIGTERM, report readiness, and wait on a
 * futex word that nothing wakes until the signal's default action ends it. */
static void futex_child(void) {
  static int word;
  prctl(PR_SET_PDEATHSIG, SIGTERM);
  char byte = 'x';
  if (write(ready[1], &byte, 1) != 1) {
    _exit(9);
  }
  for (;;) {
    syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
  }
}

/* The zombieleader mode's worker: once the leader has had time to leave
 * (it does nothing but pthread_exit; a sleep rather than a join of the main
 * thread, whose wake on KVM comes at a host-timed moment), tell the middle
 * process, then pause until the leader's parent-death signal has been
 * handled. */
static void* zombie_worker(void* arg) {
  (void)arg;
  usleep(100000);
  char byte = 'x';
  if (write(ready[1], &byte, 1) != 1) {
    _exit(9);
  }
  while (!handled) {
    pause();
  }
  _exit(7);
}

/* The zombieleader mode's child: its leader arms a caught SIGUSR1, starts
 * the worker and leaves. */
static void zombie_leader_child(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = on_usr1;
  sigaction(SIGUSR1, &action, NULL);
  prctl(PR_SET_PDEATHSIG, SIGUSR1);
  pthread_t worker;
  if (pthread_create(&worker, NULL, zombie_worker, NULL) != 0) {
    _exit(1);
  }
  pthread_exit(NULL);
}

static volatile sig_atomic_t received;

static void count_usr1(int sig) {
  (void)sig;
  received++;
}

/* The resend mode's child: arm a counted SIGUSR1, report readiness to the
 * middle process, tell the inner subreaper once the first has been handled,
 * and exit with the count once the second has. */
static void resend_child(int first) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = count_usr1;
  sigaction(SIGUSR1, &action, NULL);
  prctl(PR_SET_PDEATHSIG, SIGUSR1);
  char byte = 'x';
  if (write(ready[1], &byte, 1) != 1) {
    _exit(9);
  }
  while (received < 1) {
    pause();
  }
  if (write(first, &byte, 1) != 1) {
    _exit(9);
  }
  while (received < 2) {
    pause();
  }
  _exit(received);
}

static int resend_first[2];

/* The threadresend mode's second thread: fork the counting child, wait
 * until it has armed its signal, and leave. */
static void* resend_creator(void* arg) {
  (void)arg;
  if (fork() == 0) {
    resend_child(resend_first[1]);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(1);
  }
  return NULL;
}

static void* creator(void* arg) {
  int signal = *(int*)arg;
  child = fork();
  if (child == 0) {
    armed_child(signal);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(1);
  }
  return NULL;
}

static void on_usr1_exit(int sig) {
  (void)sig;
  _exit(3);
}

static void* exec_creator(void* arg) {
  char* self = arg;
  child = fork();
  if (child == 0) {
    signal(SIGUSR1, on_usr1_exit);
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    char byte = 'x';
    if (write(ready[1], &byte, 1) != 1) {
      _exit(9);
    }
    sleep(1);
    _exit(0);
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(8);
  }
  char* args[] = {self, "exec-after", NULL};
  execv(self, args);
  _exit(7);
}

static void* group_worker(void* arg) {
  (void)arg;
  child = fork();
  if (child == 0) {
    armed_child(SIGTERM);
  }
  for (;;) {
    pause();
  }
  return NULL;
}

static void* invalid_creator(void* arg) {
  (void)arg;
  child = fork();
  if (child == 0) {
    prctl(PR_SET_PDEATHSIG, SIGTERM);
    int rc = prctl(PR_SET_PDEATHSIG, 99);
    int error = errno, armed = 0;
    prctl(PR_GET_PDEATHSIG, &armed);
    printf("invalid: get %d einval %d\n", armed, rc == -1 && error == EINVAL);
    fflush(stdout);
    char byte = 'x';
    if (write(ready[1], &byte, 1) != 1) {
      _exit(9);
    }
    for (;;) {
      pause();
    }
  }
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    _exit(8);
  }
  return NULL;
}

/* A fresh child arms SIGUSR1, makes credential setter `which`, and exits
 * with the parent-death signal it holds afterwards. */
static int setter_keeps(int which) {
  pid_t pid = fork();
  if (pid == 0) {
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    gid_t group = 1000;
    switch (which) {
      case 0: setuid(1000); break;
      case 1: setgid(1000); break;
      case 2: setreuid(-1, 1000); break;
      case 3: setregid(-1, 1000); break;
      case 4: setresuid(-1, 1000, -1); break;
      case 5: setresgid(-1, 1000, -1); break;
      case 6: setfsuid(1000); break;
      case 7: setfsgid(1000); break;
      case 8: setreuid(1000, -1); break;
      case 9: setuid(0); break;
      case 10: setgroups(1, &group); break;
    }
    int armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    _exit(armed);
  }
  int status = 0;
  waitpid(pid, &status, 0);
  return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
}

/* The calling thread's capability sets, as capget gives them: two 32-bit
 * words of each set. */
static int capabilities(struct __user_cap_data_struct data[2]) {
  struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
  return syscall(SYS_capget, &header, data);
}

static void report(const char* mode, int status) {
  printf("%s: child %s %d\n", mode, WIFSIGNALED(status) ? "signal" : "exit",
         WIFSIGNALED(status) ? WTERMSIG(status) : WEXITSTATUS(status));
}

/* execve(path, args) with the stack pointer 64 bytes above a guard page, so
 * that no scratch is free below the red zone. Returns only if the exec
 * fails. */
static long tight_execve(const char* path, char** args) {
  char* region = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (region == MAP_FAILED || mprotect(region, 4096, PROT_NONE) != 0) {
    return -1;
  }
  long result;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[sp], %%rsp\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp\n\t"
      : "=a"(result)
      : "a"((long)SYS_execve), "D"(path), "S"(args), "d"(environ),
        [sp] "r"(region + 4096 + 64)
      : "rcx", "r11", "r12", "memory");
  return result;
}

int main(int argc, char** argv) {
  const char* mode = argc > 1 ? argv[1] : "term";
  if (!strcmp(mode, "after-secure-exec")) {
    int armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    printf("secure-exec: armed %d at_secure %lu\n", armed, getauxval(AT_SECURE));
    return 0;
  }
  if (!strcmp(mode, "after-tightexec")) {
    int armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    printf("tightexec: armed %d\n", armed);
    return 0;
  }
  if (!strcmp(mode, "tightexec")) {
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    static char* args[3];
    args[0] = argv[0];
    args[1] = "after-tightexec";
    args[2] = NULL;
    printf("tightexec: execve returned %ld\n", tight_execve(argv[0], args));
    return 1;
  }
  if (!strcmp(mode, "after-capexec")) {
    struct __user_cap_data_struct data[2];
    if (argc < 4 || capabilities(data) != 0) {
      return 2;
    }
    int bit = atoi(argv[3]);
    int armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    printf("%s: regained %d armed %d at_secure %lu\n", argv[2],
           !!(data[bit / 32].permitted & (1u << (bit % 32))), armed,
           getauxval(AT_SECURE));
    return 0;
  }
  if (!strcmp(mode, "capexec") || !strcmp(mode, "capexecall") ||
      !strcmp(mode, "tightcap")) {
    int all = !strcmp(mode, "capexecall");
    struct __user_cap_data_struct data[2];
    if (capabilities(data) != 0) {
      perror("capget");
      return 1;
    }
    int bit = -1;
    for (int candidate = 0; candidate < 64 && bit < 0; candidate++) {
      if ((data[candidate / 32].permitted & (1u << (candidate % 32))) &&
          prctl(PR_CAPBSET_READ, candidate) == 1) {
        bit = candidate;
      }
    }
    if (bit < 0) {
      fprintf(stderr, "%s: no permitted capability in the bounding set\n", mode);
      return 77;
    }
    if (all) {
      prctl(PR_SET_PDEATHSIG, SIGUSR1);
      memset(data, 0, sizeof(data));
    } else {
      data[bit / 32].permitted &= ~(1u << (bit % 32));
      data[bit / 32].effective &= ~(1u << (bit % 32));
    }
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    if (syscall(SYS_capset, &header, data) != 0) {
      perror("capset");
      return 1;
    }
    if (!all) {
      prctl(PR_SET_PDEATHSIG, SIGUSR1);
    }
    char number[16];
    snprintf(number, sizeof(number), "%d", bit);
    char* args[] = {argv[0], "after-capexec", (char*)mode, number, NULL};
    if (!strcmp(mode, "tightcap")) {
      printf("tightcap: execve returned %ld\n", tight_execve(argv[0], args));
      return 1;
    }
    execv(all ? "/proc/self/exe" : argv[0], args);
    perror("execv");
    return 1;
  }
  if (!strcmp(mode, "secure-exec")) {
    if (argc < 3) {
      return 2;
    }
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    char* args[] = {argv[2], "after-secure-exec", NULL};
    execv(argv[2], args);
    perror("execv");
    return 1;
  }
  if (!strcmp(mode, "setid")) {
    const char* names[] = {"setuid",   "setgid",   "setreuid", "setregid",
                           "setresuid", "setresgid", "setfsuid", "setfsgid",
                           "ruid",     "setuid0",  "groups"};
    printf("setid:");
    for (int which = 0; which < 11; which++) {
      printf(" %s %d", names[which], setter_keeps(which));
    }
    printf("\n");
    return 0;
  }
  if (!strcmp(mode, "exec-after")) {
    int status = 0;
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report("exec", status);
    return 0;
  }
  if (pipe(ready) != 0) {
    return 1;
  }
  if (!strcmp(mode, "exec")) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, exec_creator, argv[0]) != 0) {
      return 1;
    }
    for (;;) {
      pause();
    }
  }
  if (!strcmp(mode, "group")) {
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t middle = fork();
    if (middle == 0) {
      pthread_t worker;
      if (pthread_create(&worker, NULL, group_worker, NULL) != 0) {
        _exit(1);
      }
      char byte;
      if (read(ready[0], &byte, 1) != 1) {
        _exit(1);
      }
      _exit(0);
    }
    int status = 0;
    waitpid(middle, &status, 0);
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "invalid")) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, invalid_creator, NULL) != 0) {
      return 1;
    }
    pthread_join(thread, NULL);
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "stop") || !strcmp(mode, "realtime") ||
      !strcmp(mode, "sigchld")) {
    int signal = SIGCHLD;
    if (!strcmp(mode, "stop")) {
      signal = SIGSTOP;
    } else if (!strcmp(mode, "realtime")) {
      signal = SIGRTMIN;
    }
    errno = 0;
    int rc = prctl(PR_SET_PDEATHSIG, signal);
    int error = errno, armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    prctl(PR_SET_PDEATHSIG, 0);
    printf("%s: rc %d errno %d armed %d\n", mode, rc, error, armed == signal);
    return 0;
  }
  if (!strcmp(mode, "get")) {
    prctl(PR_SET_PDEATHSIG, SIGUSR1);
    int armed = -1;
    prctl(PR_GET_PDEATHSIG, &armed);
    pid_t pid = fork();
    if (pid == 0) {
      int inherited = -1;
      prctl(PR_GET_PDEATHSIG, &inherited);
      _exit(inherited);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("get: %d, child %d\n", armed, WEXITSTATUS(status));
    return 0;
  }
  if (!strcmp(mode, "zombieleader")) {
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t middle = fork();
    if (middle == 0) {
      if (fork() == 0) {
        zombie_leader_child();
      }
      char byte;
      if (read(ready[0], &byte, 1) != 1) {
        _exit(1);
      }
      _exit(0);
    }
    int status = 0;
    waitpid(middle, &status, 0);
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "resend")) {
    int first[2];
    if (pipe(first) != 0) {
      return 1;
    }
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t inner = fork();
    if (inner == 0) {
      prctl(PR_SET_CHILD_SUBREAPER, 1);
      pid_t middle = fork();
      if (middle == 0) {
        if (fork() == 0) {
          resend_child(first[1]);
        }
        char byte;
        if (read(ready[0], &byte, 1) != 1) {
          _exit(1);
        }
        _exit(0);
      }
      waitpid(middle, NULL, 0);
      char byte;
      if (read(first[0], &byte, 1) != 1) {
        _exit(1);
      }
      _exit(0);
    }
    int status = 0;
    waitpid(inner, &status, 0);
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "threadresend")) {
    if (pipe(resend_first) != 0) {
      return 1;
    }
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t middle = fork();
    if (middle == 0) {
      pthread_t thread;
      if (pthread_create(&thread, NULL, resend_creator, NULL) != 0) {
        _exit(1);
      }
      char byte;
      if (read(resend_first[0], &byte, 1) != 1) {
        _exit(1);
      }
      _exit(0);
    }
    int status = 0;
    waitpid(middle, &status, 0);
    if (waitpid(-1, &status, 0) <= 0) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  if (!strcmp(mode, "subreaper") || !strcmp(mode, "futex")) {
    prctl(PR_SET_CHILD_SUBREAPER, 1);
    pid_t middle = fork();
    if (middle == 0) {
      child = fork();
      if (child == 0) {
        if (!strcmp(mode, "futex")) {
          futex_child();
        }
        armed_child(SIGTERM);
      }
      char byte;
      if (read(ready[0], &byte, 1) != 1 ||
          write(ready[1], &child, sizeof(child)) != (ssize_t)sizeof(child)) {
        _exit(1);
      }
      _exit(0);
    }
    waitpid(middle, NULL, 0);
    if (read(ready[0], &child, sizeof(child)) != (ssize_t)sizeof(child)) {
      return 1;
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
      perror("waitpid");
      return 1;
    }
    report(mode, status);
    return 0;
  }
  int signal = SIGTERM, options = 0;
  if (!strcmp(mode, "caught")) {
    signal = SIGUSR1;
  } else if (!strcmp(mode, "kill")) {
    signal = SIGKILL;
  } else if (!strcmp(mode, "nothread")) {
    signal = SIGUSR1;
    options = __WNOTHREAD;
  }
  pthread_t thread;
  if (pthread_create(&thread, NULL, creator, &signal) != 0) {
    return 1;
  }
  pthread_join(thread, NULL);
  int status = 0;
  if (waitpid(child, &status, options) != child) {
    perror("waitpid");
    return 1;
  }
  report(mode, status);
  return 0;
}
