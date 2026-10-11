// A SIGKILL sent by one guest process to another
// (https://github.com/rrnewton/hermit/issues/3994).
//
// Mode "parent": process A forks C; C kills A, its parent, with SIGKILL and
// exits; main waits for A. Mode "bystander": the same, plus a bystander
// process B that takes one scheduler turn per sched_yield throughout, and C
// keeps yielding after the kill. Natively, and under Hermit, main prints how A
// ended. Mode "sigchld": main catches SIGCHLD with SA_SIGINFO; a sibling B
// kills A and then waits on a pipe, so A's notification is handled before B
// exits; main prints each notification's code, status and sender. Mode
// "pgroup": A leads its own process group, and its child A2 is in it; B, in
// main's group, sends kill(-pgid, SIGKILL); main waits for A and for B.
// Two modes kill the sender's own process, so the send cannot return: in
// "selfgroup" a non-leader thread of A tgkills A's leader while a bystander B
// takes turns; in "pgself" A, leading a group that also holds its child A2,
// sends kill(-pgid, SIGKILL) to its own group. main waits for A.
// Modes where the sender is the victim's parent (main): "parentkill" kills a
// pausing child and prints its SIGCHLD siginfo; "twice" sends SIGKILL twice,
// and the second, to the dead and unreaped child, succeeds; "zombie" waits
// for an exited child with WNOWAIT and then signals the zombie; "pipeeof"
// kills a pipe's sole writer and reads to EOF; "cleartid" kills a CLONE_VM |
// CLONE_CHILD_CLEARTID child and waits for its cleared futex word; "sigign"
// and "nocldwait" kill a child whose exit is auto-reaped; "failsend" signals
// a pid and a group that do not exist; "badtgkill" (and "badtgkill_sibling",
// with a futex-waiting sibling thread) tgkills its own thread under its
// parent's tgid, which Linux rejects with ESRCH; "intmin" sends
// kill(INT_MIN, SIGKILL), which Linux refuses with ESRCH. "zombie_term"
// SIGTERMs a zombie (0); "autoreap_term" and "nocldwait_term" SIGTERM a child
// the kernel auto-reaped (ESRCH). "bgrecord" kills a victim blocked reading a
// TCP socket, for the record-mode refusal. "sigtimedwait" blocks SIGCHLD,
// kills its child and collects the notification with sigtimedwait;
// "groupwait" does the same for a killed process group of two children, and
// "twokills" for two children killed by two separate kills; "groupasync"
// kills the group of two with SIGCHLD caught and unblocked. "sfdtwokills"
// reads the two kills' notifications from a signalfd with poll and read, and
// "wogroupwait" collects the group's with sigtimedwait into a write-only page.
// "latesfdtwokills" and "latesfdgroup" create the signalfd only after the
// kills. "sfdreadvtwokills" reads "sfdtwokills"' notifications with readv
// into one iovec, "sfdreadvgroup" reads a group's with readv into two iovecs
// that split each record, "sfdreadvselftwokills" and
// "sfdreadvselfgroup" read with readv whose one iovec lies in the first bytes
// of its own destination, "sfdreadvhighcount" reads "sfdtwokills"' with a raw
// readv whose count is (1 << 32) | 1 (Linux takes 1), "sfdpositioned" calls
// preadv and preadv2 at offset 0 on a signalfd with the iovec array in [vvar]
// (ESPIPE before the array is read), and "latesfdpreadv2group" reads "latesfdgroup"'s
// with preadv2 at the current offset (-1).
// Before the
// fix the killed process left the scheduler only when its
// own deregistration arrived, at a moment the host chose, so the committed
// schedule (and Hermit's INFO log) differed between runs.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <sys/syscall.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/uio.h>
#include <sys/socket.h>
#include <poll.h>
#include <time.h>
#include <linux/futex.h>
#include <sys/wait.h>
#include <unistd.h>

#define YIELDS 200

static volatile sig_atomic_t notifications;
static volatile sig_atomic_t first_code, first_status, first_pid;

static void on_sigchld(int signal, siginfo_t *info, void *context) {
  (void)signal;
  (void)context;
  if (notifications++ == 0) {
    first_code = info->si_code;
    first_status = info->si_status;
    first_pid = info->si_pid;
  }
}

// A survivor must have exited normally with status 0: a signal death would
// otherwise read as "exited 0".
static int report_survivor(int status) {
  if (WIFEXITED(status)) {
    printf("B exited %d\n", WEXITSTATUS(status));
    return WEXITSTATUS(status) == 0 ? 0 : 1;
  }
  printf("B killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  return 1;
}

static int sigchld_mode(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_sigchld;
  action.sa_flags = SA_SIGINFO | SA_RESTART;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  int release[2];
  if (pipe(release) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t a = fork();
  if (a == 0) {
    for (;;) {
      pause();
    }
  }
  pid_t b = fork();
  if (b == 0) {
    char byte;
    close(release[1]);
    kill(a, SIGKILL);
    _exit(read(release[0], &byte, 1) == 1 ? 0 : 4);
  }
  close(release[0]);
  int status = 0;
  if (waitpid(a, &status, 0) != a) {
    perror("waitpid A");
    return 1;
  }
  printf("A killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  printf(
      "first SIGCHLD: %s, status %d, from A: %d\n",
      first_code == CLD_KILLED ? "CLD_KILLED" : "other",
      (int)first_status,
      first_pid == a);
  if (write(release[1], "x", 1) != 1) {
    perror("write");
    return 1;
  }
  if (waitpid(b, &status, 0) != b) {
    perror("waitpid B");
    return 1;
  }
  if (report_survivor(status) != 0) {
    fflush(stdout);
    return 1;
  }
  fflush(stdout);
  return 0;
}

static int pgroup_mode(void) {
  int ready[2];
  if (pipe(ready) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t a = fork();
  if (a == 0) {
    close(ready[0]);
    if (setpgid(0, 0) != 0) {
      _exit(5);
    }
    pid_t a2 = fork();
    if (a2 == 0) {
      for (;;) {
        pause();
      }
    }
    if (write(ready[1], "x", 1) != 1) {
      _exit(6);
    }
    for (;;) {
      pause();
    }
  }
  close(ready[1]);
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    perror("read");
    return 1;
  }
  pid_t b = fork();
  if (b == 0) {
    _exit(kill(-a, SIGKILL) == 0 ? 0 : 7);
  }
  int status = 0;
  if (waitpid(a, &status, 0) != a) {
    perror("waitpid A");
    return 1;
  }
  printf("A killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  if (waitpid(b, &status, 0) != b) {
    perror("waitpid B");
    return 1;
  }
  if (report_survivor(status) != 0) {
    fflush(stdout);
    return 1;
  }
  fflush(stdout);
  return 0;
}

static pid_t leader;

static void *kill_leader(void *arg) {
  (void)arg;
  syscall(SYS_tgkill, leader, leader, SIGKILL);
  for (;;) {
    pause();
  }
  return NULL;
}

static int report_a(pid_t a) {
  int status = 0;
  if (waitpid(a, &status, 0) != a) {
    perror("waitpid A");
    return 1;
  }
  printf("A killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  return 0;
}

static int selfgroup_mode(void) {
  pid_t b = fork();
  if (b == 0) {
    for (int i = 0; i < YIELDS; i++) {
      sched_yield();
    }
    _exit(0);
  }
  pid_t a = fork();
  if (a == 0) {
    leader = getpid();
    pthread_t thread;
    if (pthread_create(&thread, NULL, kill_leader, NULL) != 0) {
      _exit(5);
    }
    for (;;) {
      pause();
    }
  }
  if (report_a(a) != 0) {
    return 1;
  }
  int status = 0;
  if (waitpid(b, &status, 0) != b) {
    perror("waitpid B");
    return 1;
  }
  if (report_survivor(status) != 0) {
    fflush(stdout);
    return 1;
  }
  fflush(stdout);
  return 0;
}

static int pgself_mode(void) {
  pid_t a = fork();
  if (a == 0) {
    if (setpgid(0, 0) != 0) {
      _exit(5);
    }
    pid_t a2 = fork();
    if (a2 == 0) {
      for (;;) {
        pause();
      }
    }
    kill(-getpid(), SIGKILL);
    _exit(6); /* not reached */
  }
  if (report_a(a) != 0) {
    return 1;
  }
  fflush(stdout);
  return 0;
}

static int catch_sigchld(int flags) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_sigchld;
  action.sa_flags = SA_SIGINFO | SA_RESTART | flags;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  return 0;
}

static pid_t pausing_child(void) {
  pid_t child = fork();
  if (child == 0) {
    for (;;) {
      pause();
    }
  }
  return child;
}

static void print_notification(pid_t child) {
  printf(
      "SIGCHLD: %s, status %d, from child: %d\n",
      first_code == CLD_KILLED   ? "CLD_KILLED"
          : first_code == CLD_EXITED ? "CLD_EXITED"
                                     : "other",
      (int)first_status,
      first_pid == child);
}

static int parentkill_mode(int twice) {
  if (catch_sigchld(0) != 0) {
    return 1;
  }
  pid_t child = pausing_child();
  int first = kill(child, SIGKILL);
  int second = twice ? kill(child, SIGKILL) : 0;
  int second_errno = second == 0 ? 0 : errno;
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf("kill: %d, second kill: %d errno %d\n", first, second, second_errno);
  printf("child killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  print_notification(child);
  fflush(stdout);
  return 0;
}

static int zombie_mode(void) {
  if (catch_sigchld(0) != 0) {
    return 1;
  }
  pid_t child = fork();
  if (child == 0) {
    _exit(7);
  }
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  if (waitid(P_PID, child, &info, WEXITED | WNOWAIT) != 0) {
    perror("waitid");
    return 1;
  }
  int sent = kill(child, SIGKILL);
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  printf("kill of the zombie: %d\n", sent);
  printf("child exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
  print_notification(child);
  fflush(stdout);
  return 0;
}

static int pipeeof_mode(void) {
  int fds[2];
  if (pipe(fds) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t writer = fork();
  if (writer == 0) {
    close(fds[0]);
    if (write(fds[1], "w", 1) != 1) {
      _exit(5);
    }
    for (;;) {
      pause();
    }
  }
  close(fds[1]);
  char byte;
  if (read(fds[0], &byte, 1) != 1) {
    perror("read");
    return 1;
  }
  kill(writer, SIGKILL);
  ssize_t eof = read(fds[0], &byte, 1);
  int status = 0;
  waitpid(writer, &status, 0);
  printf("read after the kill: %zd\n", eof);
  printf("writer killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  fflush(stdout);
  return 0;
}

static char cleartid_stack[64 * 1024] __attribute__((aligned(16)));
static volatile int cleartid_word = 1;

static int cleartid_child(void *arg) {
  (void)arg;
  for (;;) {
    pause();
  }
  return 0;
}

static int cleartid_mode(void) {
  pid_t child = clone(
      cleartid_child,
      cleartid_stack + sizeof(cleartid_stack),
      CLONE_VM | CLONE_CHILD_CLEARTID | SIGCHLD,
      NULL,
      NULL,
      NULL,
      (int *)&cleartid_word);
  if (child < 0) {
    perror("clone");
    return 1;
  }
  kill(child, SIGKILL);
  while (cleartid_word != 0) {
    syscall(SYS_futex, &cleartid_word, FUTEX_WAIT, cleartid_word, NULL, NULL, 0);
  }
  int status = 0;
  waitpid(child, &status, 0);
  printf("futex word: %d\n", cleartid_word);
  printf("child killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  fflush(stdout);
  return 0;
}

static int autoreap_mode(int nocldwait) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = nocldwait ? SIG_DFL : SIG_IGN;
  action.sa_flags = nocldwait ? SA_NOCLDWAIT : 0;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  pid_t child = pausing_child();
  int sent = kill(child, SIGKILL);
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  int wait_errno = waited < 0 ? errno : 0;
  printf("kill: %d, waitpid: %d errno %s\n", sent, waited < 0 ? -1 : 0,
         wait_errno == ECHILD ? "ECHILD" : "other");
  fflush(stdout);
  return 0;
}

// tgkill with a thread outside the named group: Linux returns ESRCH and
// nothing dies. With a sibling thread parked on a futex, the process must
// still be able to wake it and exit normally.
static volatile int badtgkill_word;

static void *badtgkill_sibling(void *arg) {
  (void)arg;
  while (badtgkill_word == 0) {
    syscall(SYS_futex, &badtgkill_word, FUTEX_WAIT, 0, NULL, NULL, 0);
  }
  return NULL;
}

static int badtgkill_child(int with_sibling) {
  pthread_t sibling;
  if (with_sibling && pthread_create(&sibling, NULL, badtgkill_sibling, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }
  long sent = syscall(SYS_tgkill, getppid(), syscall(SYS_gettid), SIGKILL);
  int sent_errno = errno;
  if (with_sibling) {
    badtgkill_word = 1;
    syscall(SYS_futex, &badtgkill_word, FUTEX_WAKE, 1, NULL, NULL, 0);
    pthread_join(sibling, NULL);
  }
  printf("tgkill(parent, self): %ld %s; still running\n", sent,
         sent_errno == ESRCH ? "ESRCH" : "other");
  fflush(stdout);
  return 0;
}

// The sender is a forked child, so its parent (main) is a live process whose
// group does not contain the sender's thread.
static int badtgkill_mode(int with_sibling) {
  fflush(stdout);
  pid_t child = fork();
  if (child == 0) {
    _exit(badtgkill_child(with_sibling));
  }
  int status = 0;
  if (waitpid(child, &status, 0) != child) {
    perror("waitpid");
    return 1;
  }
  return report_survivor(status);
}

static int intmin_mode(void) {
  int sent = kill(-2147483647 - 1, SIGKILL);
  int sent_errno = errno;
  printf("kill(INT_MIN): %d %s; still running\n", sent, sent_errno == ESRCH ? "ESRCH" : "other");
  fflush(stdout);
  return 0;
}

// A non-SIGKILL signal to an exited child: a zombie is signalled
// successfully, a child the kernel auto-reaped is gone (ESRCH).
static int exited_signal_mode(int how) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_handler = how == 1 ? SIG_IGN : SIG_DFL;
  action.sa_flags = how == 2 ? SA_NOCLDWAIT : 0;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  int done[2];
  if (pipe(done) != 0) {
    perror("pipe");
    return 1;
  }
  pid_t child = fork();
  if (child == 0) {
    close(done[0]);
    _exit(0);
  }
  close(done[1]);
  char byte;
  if (read(done[0], &byte, 1) != 0) {
    perror("read");
    return 1;
  }
  if (how == 0) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    if (waitid(P_PID, child, &info, WEXITED | WNOWAIT) != 0) {
      perror("waitid");
      return 1;
    }
  } else {
    struct timespec pause_time = {0, 200000000};
    nanosleep(&pause_time, NULL);
  }
  int sent = kill(child, SIGTERM);
  int sent_errno = errno;
  printf("kill(child, SIGTERM): %d %s\n", sent, sent == 0 ? "ok" : sent_errno == ESRCH ? "ESRCH" : "other");
  if (how == 0) {
    int status = 0;
    waitpid(child, &status, 0);
  }
  fflush(stdout);
  return 0;
}

// The parent blocks SIGCHLD, kills its child and collects the notification
// synchronously with sigtimedwait: it must see Linux's siginfo.
static int sigtimedwait_mode(void) {
  sigset_t chld;
  sigemptyset(&chld);
  sigaddset(&chld, SIGCHLD);
  if (sigprocmask(SIG_BLOCK, &chld, NULL) != 0) {
    perror("sigprocmask");
    return 1;
  }
  pid_t child = pausing_child();
  kill(child, SIGKILL);
  siginfo_t info;
  memset(&info, 0, sizeof(info));
  struct timespec limit = {10, 0};
  int signal = sigtimedwait(&chld, &info, &limit);
  int status = 0;
  waitpid(child, &status, 0);
  printf("sigtimedwait: %s, %s, status %d, from child: %d\n",
         signal == SIGCHLD ? "SIGCHLD" : "other",
         info.si_code == CLD_KILLED ? "CLD_KILLED" : "other",
         info.si_status, info.si_pid == child);
  fflush(stdout);
  return 0;
}

// The parent catches and blocks SIGCHLD, kills a process group of two of its
// children, and collects the notifications with sigtimedwait. Linux may
// coalesce them into one or deliver two, in either order; each consumed one
// must name a killed child with CLD_KILLED and status 9 (shape from the Codex
// re-check of https://github.com/rrnewton/hermit/pull/4031).
static void groupwait_caught(int signal, siginfo_t *info, void *context) {
  (void)signal;
  (void)info;
  (void)context;
}

static void groupwait_child(int fd, pid_t group) {
  if (setpgid(0, group) != 0 || write(fd, "r", 1) != 1) {
    _exit(77);
  }
  for (;;) {
    pause();
  }
}

// How groupwait_mode collects the notifications.
enum consumer { SIGTIMEDWAIT, WRITE_ONLY_SIGTIMEDWAIT, SIGNALFD, LATE_SIGNALFD };

// Which call reads a signalfd record: read, readv into one iovec, readv into
// two iovecs that split the record, readv whose one iovec lies in the first
// bytes of its own destination, raw readv with the count (1 << 32) | 1, which
// Linux reads as 1, or preadv2 at the current offset.
enum signalfd_read {
  READ_PLAIN,
  READ_READV,
  READ_READV_SPLIT,
  READ_READV_SELF,
  READ_READV_HIGH_COUNT,
  READ_PREADV2
};
static enum signalfd_read signalfd_read_call = READ_PLAIN;

static ssize_t read_signalfd_record(int fd, struct signalfd_siginfo *info) {
  struct iovec whole = {info, sizeof(*info)};
  struct iovec split[2] = {{info, 40}, {(char *)info + 40, sizeof(*info) - 40}};
  switch (signalfd_read_call) {
  case READ_READV:
    return readv(fd, &whole, 1);
  case READ_READV_SPLIT:
    return readv(fd, split, 2);
  case READ_READV_SELF: {
    // The iovec array is the first 16 bytes of its own destination: Linux
    // imports it before the record overwrites it.
    struct iovec *own = (struct iovec *)info;
    own->iov_base = info;
    own->iov_len = sizeof(*info);
    return readv(fd, own, 1);
  }
  case READ_READV_HIGH_COUNT:
    // Linux takes only the count's low 32 bits: this is one iovec.
    return syscall(SYS_readv, fd, &whole, (1UL << 32) | 1);
  case READ_PREADV2:
    return preadv2(fd, &whole, 1, -1, 0);
  default:
    return read(fd, info, sizeof(*info));
  }
}

// Take one SIGCHLD notification, waiting up to `wait`: 1 with its pid, code
// and status, 0 for none, -1 on failure.
static int take_notification(enum consumer consumer, sigset_t *set, int fd, siginfo_t *page,
                             const struct timespec *wait, pid_t *pid, int *code, int *status) {
  if (consumer == SIGNALFD || consumer == LATE_SIGNALFD) {
    struct pollfd ready = {.fd = fd, .events = POLLIN};
    int found = poll(&ready, 1, (int)(wait->tv_sec * 1000 + wait->tv_nsec / 1000000));
    if (found <= 0) {
      return found;
    }
    struct signalfd_siginfo info;
    if (read_signalfd_record(fd, &info) != (ssize_t)sizeof(info) || info.ssi_signo != SIGCHLD) {
      return -1;
    }
    *pid = (pid_t)info.ssi_pid;
    *code = info.ssi_code;
    *status = info.ssi_status;
    return 1;
  }
  siginfo_t local;
  siginfo_t *info = consumer == WRITE_ONLY_SIGTIMEDWAIT ? page : &local;
  memset(&local, 0, sizeof(local));
  errno = 0;
  int signal = sigtimedwait(set, info, wait);
  if (signal == -1 && errno == EAGAIN) {
    return 0;
  }
  if (signal != SIGCHLD) {
    return -1;
  }
  if (consumer == WRITE_ONLY_SIGTIMEDWAIT) {
    if (mprotect(page, 4096, PROT_READ | PROT_WRITE) != 0) {
      return -1;
    }
    local = *page;
    if (mprotect(page, 4096, PROT_WRITE) != 0) {
      return -1;
    }
  }
  *pid = local.si_pid;
  *code = local.si_code;
  *status = local.si_status;
  return 1;
}

static int groupwait_mode(int separate, enum consumer consumer) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = groupwait_caught;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  if (sigaction(SIGCHLD, &action, NULL) != 0 || sigprocmask(SIG_BLOCK, &set, NULL) != 0) {
    return 77;
  }
  int ready[2];
  if (pipe(ready) != 0) {
    return 77;
  }
  char byte;
  pid_t a = fork();
  if (a == 0) {
    close(ready[0]);
    groupwait_child(ready[1], 0);
  }
  if (read(ready[0], &byte, 1) != 1) {
    return 77;
  }
  pid_t c = fork();
  if (c == 0) {
    close(ready[0]);
    groupwait_child(ready[1], a);
  }
  if (read(ready[0], &byte, 1) != 1) {
    return 77;
  }
  // A late signalfd is created only after the kills, when their
  // notifications are already pending.
  int fd = consumer == SIGNALFD ? signalfd(-1, &set, 0) : -1;
  siginfo_t *page = consumer == WRITE_ONLY_SIGTIMEDWAIT
                        ? mmap(NULL, 4096, PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
                        : NULL;
  if ((consumer == SIGNALFD && fd < 0) || page == MAP_FAILED) {
    return 77;
  }
  if (separate ? kill(a, SIGKILL) != 0 || kill(c, SIGKILL) != 0 : kill(-a, SIGKILL) != 0) {
    return 77;
  }
  if (consumer == LATE_SIGNALFD && (fd = signalfd(-1, &set, 0)) < 0) {
    return 77;
  }
  // An event loop reads its signalfd without blocking.
  if ((consumer == SIGNALFD || consumer == LATE_SIGNALFD) &&
      fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK) != 0) {
    return 77;
  }
  int ok = 1;
  int total = 0;
  struct timespec first = {1, 0};
  struct timespec zero = {0, 0};
  // A signalfd event loop polls with a timeout, so a later notification
  // has time to arrive, as in the Claude re-check #3 probe.
  struct timespec later = {0, 100 * 1000 * 1000};
  for (int i = 0; i < 4; i++) {
    pid_t pid = 0;
    int code = 0;
    int status = 0;
    const struct timespec *wait =
        i == 0 ? &first : (consumer == SIGNALFD || consumer == LATE_SIGNALFD) ? &later : &zero;
    int taken = take_notification(consumer, &set, fd, page, wait, &pid, &code, &status);
    if (taken == 0) {
      break;
    }
    total++;
    if (taken < 0 || (pid != a && pid != c) || code != CLD_KILLED || status != SIGKILL) {
      ok = 0;
    }
  }
  int status_a = 0;
  int status_c = 0;
  ok &= waitpid(a, &status_a, 0) == a && waitpid(c, &status_c, 0) == c;
  ok &= WIFSIGNALED(status_a) && WTERMSIG(status_a) == SIGKILL;
  ok &= WIFSIGNALED(status_c) && WTERMSIG(status_c) == SIGKILL;
  ok &= total >= 1 && total <= 2;
  printf("%s%s notifications genuine: %d\n",
         consumer == SIGNALFD                  ? "signalfd "
         : consumer == LATE_SIGNALFD           ? "late signalfd "
         : consumer == WRITE_ONLY_SIGTIMEDWAIT ? "write-only "
                                               : "",
         separate ? "two kills" : "group", ok);
  fflush(stdout);
  return ok ? 0 : 42;
}

// The handler of "groupasync": records each delivery.
static volatile sig_atomic_t groupasync_count;
static volatile pid_t groupasync_pids[8];
static volatile int groupasync_codes[8];
static volatile int groupasync_statuses[8];

static void groupasync_caught(int signal, siginfo_t *info, void *context) {
  (void)signal;
  (void)context;
  int i = groupasync_count;
  if (i < 8) {
    groupasync_pids[i] = info->si_pid;
    groupasync_codes[i] = info->si_code;
    groupasync_statuses[i] = info->si_status;
  }
  groupasync_count = i + 1;
}

// The parent catches SIGCHLD, unblocked, kills a process group of two of its
// children and reaps both. Linux delivers one or two notifications, either
// child first; each must name a killed child with CLD_KILLED and status 9
// (shape from the Claude re-check #2 of
// https://github.com/rrnewton/hermit/pull/4031).
static int groupasync_mode(void) {
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = groupasync_caught;
  action.sa_flags = SA_SIGINFO | SA_RESTART;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    return 77;
  }
  int ready[2];
  if (pipe(ready) != 0) {
    return 77;
  }
  char byte;
  pid_t a = fork();
  if (a == 0) {
    close(ready[0]);
    groupwait_child(ready[1], 0);
  }
  if (read(ready[0], &byte, 1) != 1) {
    return 77;
  }
  pid_t c = fork();
  if (c == 0) {
    close(ready[0]);
    groupwait_child(ready[1], a);
  }
  if (read(ready[0], &byte, 1) != 1) {
    return 77;
  }
  if (kill(-a, SIGKILL) != 0) {
    return 77;
  }
  int status_a = 0;
  int status_c = 0;
  while (waitpid(a, &status_a, 0) == -1 && errno == EINTR) {
  }
  while (waitpid(c, &status_c, 0) == -1 && errno == EINTR) {
  }
  int total = groupasync_count;
  int ok = total >= 1 && total <= 2;
  for (int i = 0; i < total && i < 8; i++) {
    if ((groupasync_pids[i] != a && groupasync_pids[i] != c) ||
        groupasync_codes[i] != CLD_KILLED || groupasync_statuses[i] != SIGKILL) {
      ok = 0;
    }
  }
  ok &= WIFSIGNALED(status_a) && WTERMSIG(status_a) == SIGKILL;
  ok &= WIFSIGNALED(status_c) && WTERMSIG(status_c) == SIGKILL;
  printf("group handler notifications genuine: %d\n", ok);
  fflush(stdout);
  return ok ? 0 : 42;
}

// A victim blocked reading an accepted TCP socket, a background operation in
// record mode, is SIGKILLed: the recording must be refused at replay.
static int bgrecord_mode(void) {
  int ready[2];
  if (pipe(ready) != 0) {
    return 77;
  }
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in address = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  if (listener < 0 || bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
      listen(listener, 1) != 0) {
    return 77;
  }
  socklen_t length = sizeof(address);
  if (getsockname(listener, (struct sockaddr *)&address, &length) != 0) {
    return 77;
  }
  int peer = socket(AF_INET, SOCK_STREAM, 0);
  if (peer < 0 || connect(peer, (struct sockaddr *)&address, length) != 0) {
    return 77;
  }
  int server = accept(listener, NULL, NULL);
  if (server < 0) {
    return 77;
  }
  pid_t victim = fork();
  if (victim == 0) {
    close(listener);
    close(peer);
    close(ready[0]);
    if (write(ready[1], "r", 1) != 1) {
      _exit(77);
    }
    char byte;
    _exit(read(server, &byte, 1) == 1 ? 42 : 43);
  }
  close(server);
  close(ready[1]);
  char byte;
  if (read(ready[0], &byte, 1) != 1) {
    return 77;
  }
  for (int i = 0; i < 40; i++) {
    sched_yield();
  }
  kill(victim, SIGKILL);
  int status = 0;
  waitpid(victim, &status, 0);
  printf("victim killed by signal %d\n", WIFSIGNALED(status) ? WTERMSIG(status) : -1);
  fflush(stdout);
  return 0;
}

static int failsend_mode(void) {
  int to_pid = kill(32000, SIGKILL);
  int pid_errno = errno;
  int to_group = kill(-32000, SIGKILL);
  int group_errno = errno;
  printf("kill(32000): %d %s, kill(-32000): %d %s\n",
         to_pid, pid_errno == ESRCH ? "ESRCH" : "other",
         to_group, group_errno == ESRCH ? "ESRCH" : "other");
  fflush(stdout);
  return 0;
}

/* The "sfdpositioned" mode (see the header). */
static int signalfd_positioned_mode(void) {
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGCHLD);
  sigprocmask(SIG_BLOCK, &set, NULL);
  int fd = signalfd(-1, &set, SFD_NONBLOCK);
  if (fd < 0) return 2;
  FILE *maps = fopen("/proc/self/maps", "r");
  if (maps == NULL) return 2;
  char line[512];
  unsigned long vvar = 0;
  while (fgets(line, sizeof line, maps) != NULL) {
    if (strstr(line, "[vvar]") != NULL) {
      vvar = strtoul(line, NULL, 16);
      break;
    }
  }
  fclose(maps);
  if (vvar == 0) {
    printf("no [vvar] mapping\n");
    return 0;
  }
  long rc = syscall(SYS_preadv, fd, (void *)vvar, 1, 0, 0);
  printf("preadv at offset 0: %ld %s\n", rc, rc < 0 ? strerrorname_np(errno) : "-");
  rc = syscall(SYS_preadv2, fd, (void *)vvar, 1, 0, 0, 0);
  printf("preadv2 at offset 0: %ld %s\n", rc, rc < 0 ? strerrorname_np(errno) : "-");
  return 0;
}

int main(int argc, char **argv) {
  const char *mode = argc > 1 ? argv[1] : "";
  if (strcmp(mode, "parentkill") == 0) {
    return parentkill_mode(0);
  }
  if (strcmp(mode, "twice") == 0) {
    return parentkill_mode(1);
  }
  if (strcmp(mode, "zombie") == 0) {
    return zombie_mode();
  }
  if (strcmp(mode, "pipeeof") == 0) {
    return pipeeof_mode();
  }
  if (strcmp(mode, "cleartid") == 0) {
    return cleartid_mode();
  }
  if (strcmp(mode, "sigign") == 0) {
    return autoreap_mode(0);
  }
  if (strcmp(mode, "nocldwait") == 0) {
    return autoreap_mode(1);
  }
  if (strcmp(mode, "failsend") == 0) {
    return failsend_mode();
  }
  if (strcmp(mode, "badtgkill") == 0) {
    return badtgkill_mode(0);
  }
  if (strcmp(mode, "badtgkill_sibling") == 0) {
    return badtgkill_mode(1);
  }
  if (strcmp(mode, "intmin") == 0) {
    return intmin_mode();
  }
  if (strcmp(mode, "zombie_term") == 0) {
    return exited_signal_mode(0);
  }
  if (strcmp(mode, "autoreap_term") == 0) {
    return exited_signal_mode(1);
  }
  if (strcmp(mode, "nocldwait_term") == 0) {
    return exited_signal_mode(2);
  }
  if (strcmp(mode, "bgrecord") == 0) {
    return bgrecord_mode();
  }
  if (strcmp(mode, "sigtimedwait") == 0) {
    return sigtimedwait_mode();
  }
  if (strcmp(mode, "groupwait") == 0) {
    return groupwait_mode(0, SIGTIMEDWAIT);
  }
  if (strcmp(mode, "twokills") == 0) {
    return groupwait_mode(1, SIGTIMEDWAIT);
  }
  if (strcmp(mode, "sfdtwokills") == 0) {
    return groupwait_mode(1, SIGNALFD);
  }
  if (strcmp(mode, "latesfdtwokills") == 0) {
    return groupwait_mode(1, LATE_SIGNALFD);
  }
  if (strcmp(mode, "latesfdgroup") == 0) {
    return groupwait_mode(0, LATE_SIGNALFD);
  }
  if (strcmp(mode, "wogroupwait") == 0) {
    return groupwait_mode(0, WRITE_ONLY_SIGTIMEDWAIT);
  }
  if (strcmp(mode, "groupasync") == 0) {
    return groupasync_mode();
  }
  if (strcmp(mode, "sfdreadvtwokills") == 0) {
    signalfd_read_call = READ_READV;
    return groupwait_mode(1, SIGNALFD);
  }
  if (strcmp(mode, "sfdreadvgroup") == 0) {
    signalfd_read_call = READ_READV_SPLIT;
    return groupwait_mode(0, SIGNALFD);
  }
  if (strcmp(mode, "sfdreadvselftwokills") == 0) {
    signalfd_read_call = READ_READV_SELF;
    return groupwait_mode(1, SIGNALFD);
  }
  if (strcmp(mode, "sfdreadvselfgroup") == 0) {
    signalfd_read_call = READ_READV_SELF;
    return groupwait_mode(0, SIGNALFD);
  }
  if (strcmp(mode, "sfdreadvhighcount") == 0) {
    signalfd_read_call = READ_READV_HIGH_COUNT;
    return groupwait_mode(1, SIGNALFD);
  }
  if (strcmp(mode, "sfdpositioned") == 0) {
    return signalfd_positioned_mode();
  }
  if (strcmp(mode, "latesfdpreadv2group") == 0) {
    signalfd_read_call = READ_PREADV2;
    return groupwait_mode(0, LATE_SIGNALFD);
  }
  if (argc > 1 && strcmp(argv[1], "selfgroup") == 0) {
    return selfgroup_mode();
  }
  if (argc > 1 && strcmp(argv[1], "pgself") == 0) {
    return pgself_mode();
  }
  if (argc > 1 && strcmp(argv[1], "sigchld") == 0) {
    return sigchld_mode();
  }
  if (argc > 1 && strcmp(argv[1], "pgroup") == 0) {
    return pgroup_mode();
  }
  int bystander = argc > 1 && strcmp(argv[1], "bystander") == 0;
  pid_t b = -1;
  if (bystander) {
    b = fork();
    if (b < 0) {
      perror("fork");
      return 1;
    }
    if (b == 0) {
      for (int i = 0; i < YIELDS; i++) {
        sched_yield();
      }
      _exit(0);
    }
  }
  pid_t a = fork();
  if (a < 0) {
    perror("fork");
    return 1;
  }
  if (a == 0) {
    pid_t c = fork();
    if (c == 0) {
      kill(getppid(), SIGKILL);
      if (bystander) {
        for (int i = 0; i < YIELDS; i++) {
          sched_yield();
        }
      }
      _exit(0);
    }
    for (;;) {
      pause();
    }
  }
  int status = 0;
  if (waitpid(a, &status, 0) != a) {
    perror("waitpid A");
    return 1;
  }
  if (WIFSIGNALED(status)) {
    printf("A killed by signal %d\n", WTERMSIG(status));
  } else {
    printf("A exited %d\n", WEXITSTATUS(status));
  }
  if (bystander) {
    int bystander_status = 0;
    if (waitpid(b, &bystander_status, 0) != b) {
      perror("waitpid B");
      return 1;
    }
    if (report_survivor(bystander_status) != 0) {
      fflush(stdout);
      return 1;
    }
  }
  /* C, A's orphan, goes to the container's init; main does not wait for it. */
  fflush(stdout);
  return 0;
}
