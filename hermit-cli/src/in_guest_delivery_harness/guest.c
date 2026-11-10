// The guest of the in-guest delivery harness (in_guest_delivery_harness.rs):
// one mode per row, each printing what the guest observes, so that the
// harness compares a run under the forced gate with the same run natively.
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define BIT(s) (1ULL << ((s) - 1))
#define SENTINEL 0x5a5a5a5a5a5a5a5aULL

static long result_of(long r) { return r < 0 ? -errno : r; }

static long mask_call(int how, const uint64_t *set, uint64_t *old, size_t size) {
  return result_of(syscall(SYS_rt_sigprocmask, how, set, old, size));
}

static uint64_t mask_now(void) {
  uint64_t old = 0;
  mask_call(SIG_BLOCK, NULL, &old, 8);
  return old;
}

static uint64_t pending_now(void) {
  uint64_t set = 0;
  syscall(SYS_rt_sigpending, &set, 8);
  return set;
}

static void raise_self(int sig) { syscall(SYS_tgkill, getpid(), gettid(), sig); }

static long take(int sig, long seconds) {
  uint64_t set = BIT(sig);
  struct timespec timeout = {seconds, 0};
  return result_of(syscall(SYS_rt_sigtimedwait, &set, NULL, &timeout, 8));
}

static uint64_t sigblk_of(pid_t pid) {
  char path[64], line[256];
  snprintf(path, sizeof path, "/proc/%d/status", pid);
  FILE *f = fopen(path, "r");
  uint64_t value = 0;
  if (!f) return 0;
  while (fgets(line, sizeof line, f))
    if (!strncmp(line, "SigBlk:", 7)) value = strtoull(line + 7, NULL, 16);
  fclose(f);
  return value;
}

static void *page(int protection) {
  return mmap(NULL, 4096, protection, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

static void sleep_ms(long ms) {
  struct timespec t = {0, ms * 1000 * 1000};
  nanosleep(&t, NULL);
}

static int reap(pid_t child) {
  int status = 0;
  waitpid(child, &status, 0);
  return status;
}

// The managed wait's published mask: the child reads its parent's SigBlk
// while the parent waits in wait4.
static int mode_wait4(void) {
  uint64_t usr1 = BIT(SIGUSR1);
  mask_call(SIG_SETMASK, &usr1, NULL, 8);
  pid_t parent = getpid();
  pid_t child = fork();
  if (child == 0) {
    sleep_ms(20);
    uint64_t seen = sigblk_of(parent);
    printf("child-reads-parent-sigblk=%#llx\n", (unsigned long long)seen);
    fflush(stdout);
    _exit(7);
  }
  int status = 0;
  pid_t waited = wait4(child, &status, 0, NULL);
  printf("wait4=%s status=%d mask=%#llx\n", waited == child ? "child" : "other",
         WEXITSTATUS(status), (unsigned long long)mask_now());
  return 0;
}

// D37's removal through a Tool injection: an ignored SIGURG queued while
// blocked, then masked waits whose temporary mask lets it in.
static int mode_d37(void) {
  uint64_t urg = BIT(SIGURG);
  mask_call(SIG_SETMASK, &urg, NULL, 8);
  raise_self(SIGURG);
  uint64_t before = pending_now();
  printf("urg-before=%#llx\n", (unsigned long long)before);
  sigset_t none;
  sigemptyset(&none);
  struct timespec zero = {0, 0};
  int r = ppoll(NULL, 0, &zero, &none);
  int e = r < 0 ? errno : 0;
  uint64_t after = pending_now();
  printf("ppoll-masked-zero=%d errno=%d urg-after=%#llx\n", r, e, (unsigned long long)after);
  raise_self(SIGURG);
  int ep = epoll_create1(EPOLL_CLOEXEC);
  struct epoll_event event;
  r = epoll_pwait(ep, &event, 1, 1000, &none);
  e = r < 0 ? errno : 0;
  after = pending_now();
  printf("epoll_pwait-masked=%d errno=%d urg-after=%#llx\n", r, e, (unsigned long long)after);
  close(ep);
  return 0;
}

// A masked wait that parks while another process sends an ignored signal its
// temporary mask lets in and the guest's own mask blocks: Linux discards it
// at send, so nothing is pending after. First a wait that only times out;
// then one that another process's write ends right after its send, so that
// the send and the readiness both come before the parked wait's next probe.
// That process sends only once the waiter's SigBlk shows the temporary mask:
// were the send and the write both before the call, Linux would return the
// ready descriptor and keep the signal pending.
static int mode_r2(void) {
  uint64_t urg = BIT(SIGURG);
  mask_call(SIG_SETMASK, &urg, NULL, 8);
  pid_t parent = getpid();
  pid_t child = fork();
  if (child == 0) {
    kill(parent, SIGURG);
    _exit(0);
  }
  sigset_t none;
  sigemptyset(&none);
  struct timespec wait = {0, 200 * 1000 * 1000};
  int r = ppoll(NULL, 0, &wait, &none);
  int e = r < 0 ? errno : 0;
  int status = reap(child);
  uint64_t after = pending_now();
  printf("ppoll-parked=%d errno=%d child=%d pending-after=%#llx\n", r, e, WEXITSTATUS(status),
         (unsigned long long)after);
  int fds[2];
  if (pipe(fds)) return 4;
  child = fork();
  if (child == 0) {
    close(fds[0]);
    int polls = 0;
    while (sigblk_of(parent) & urg) {
      if (++polls == 5000) _exit(2);
      sleep_ms(1);
    }
    kill(parent, SIGURG);
    if (write(fds[1], "x", 1) != 1) _exit(1);
    _exit(0);
  }
  close(fds[1]);
  struct pollfd ready = {fds[0], POLLIN, 0};
  struct timespec longer = {5, 0};
  r = ppoll(&ready, 1, &longer, &none);
  e = r < 0 ? errno : 0;
  status = reap(child);
  after = pending_now();
  // POLLHUP depends on whether the writer has exited by then, as it may on Linux.
  printf("ppoll-ready=%d errno=%d readable=%d child=%d pending-after=%#llx\n", r, e,
         !!(ready.revents & POLLIN), WEXITSTATUS(status), (unsigned long long)after);
  close(fds[0]);
  return 0;
}

// The guest's own sigtimedwait with a timeout, which the Tool polls with
// zero-timeout injections: one woken by another process, one with the signal
// already pending.
static int mode_sigtimedwait(void) {
  uint64_t both = BIT(SIGUSR1) | BIT(SIGUSR2);
  mask_call(SIG_SETMASK, &both, NULL, 8);
  pid_t parent = getpid();
  pid_t child = fork();
  if (child == 0) {
    sleep_ms(10);
    kill(parent, SIGUSR1);
    _exit(0);
  }
  long woken = take(SIGUSR1, 5);
  reap(child);
  raise_self(SIGUSR2);
  long pending = take(SIGUSR2, 5);
  printf("sigtimedwait-woken=%ld sigtimedwait-pending=%ld pending-after=%#llx\n", woken, pending,
         (unsigned long long)pending_now());
  return 0;
}

static void row(const char *name, int how, const uint64_t *set, uint64_t *old, size_t size) {
  long r = mask_call(how, set, old, size);
  printf("%s result=%ld mask=%#llx\n", name, r, (unsigned long long)mask_now());
}

// rt_sigprocmask and rt_sigpending on the guest's mask, in Linux's order,
// and the calls that must still run under the gate.
static int mode_maskcalls(void) {
  uint64_t usr1 = BIT(SIGUSR1), usr2 = BIT(SIGUSR2), hup = BIT(SIGHUP);
  uint64_t term_kill = BIT(SIGTERM) | BIT(SIGKILL) | BIT(SIGSTOP);
  uint64_t *unreadable = page(PROT_NONE);
  uint64_t *read_only = page(PROT_READ);
  uint64_t old = SENTINEL;
  row("setmask-usr1", SIG_SETMASK, &usr1, &old, 8);
  printf("  old=%#llx\n", (unsigned long long)old);
  row("block-term-kill-stop", SIG_BLOCK, &term_kill, &old, 8);
  old = SENTINEL;
  row("invalid-how-with-set", 99, &hup, &old, 8);
  printf("  old=%#llx\n", (unsigned long long)old);
  old = SENTINEL;
  row("invalid-how-null-set", 99, NULL, &old, 8);
  printf("  old=%#llx\n", (unsigned long long)old);
  old = SENTINEL;
  row("invalid-how-unreadable-set", 99, unreadable, &old, 8);
  printf("  old=%#llx\n", (unsigned long long)old);
  row("size-4", SIG_BLOCK, &hup, &old, 4);
  row("unwritable-old", SIG_BLOCK, &hup, read_only, 8);
  uint64_t both = usr1 | usr2;
  row("setmask-usr1-usr2", SIG_SETMASK, &both, NULL, 8);
  raise_self(SIGUSR1);
  uint64_t set = SENTINEL;
  long r = result_of(syscall(SYS_rt_sigpending, &set, 8));
  printf("pending-8 result=%ld set=%#llx\n", r, (unsigned long long)set);
  r = result_of(syscall(SYS_rt_sigpending, unreadable, 0));
  printf("pending-0-unwritable result=%ld\n", r);
  r = result_of(syscall(SYS_rt_sigpending, &set, 9));
  printf("pending-9 result=%ld\n", r);
  r = result_of(syscall(SYS_rt_sigpending, read_only, 8));
  printf("pending-unwritable result=%ld\n", r);
  long t1 = take(SIGUSR1, 0);
  long t2 = take(SIGUSR1, 0);
  printf("sigtimedwait-zero taken=%ld again=%ld\n", t1, t2);
  raise_self(SIGUSR2);
  sigset_t s2;
  sigemptyset(&s2);
  sigaddset(&s2, SIGUSR2);
  int fd = signalfd(-1, &s2, SFD_NONBLOCK | SFD_CLOEXEC);
  struct signalfd_siginfo info;
  ssize_t n = read(fd, &info, sizeof info);
  printf("signalfd read=%zd signo=%u\n", n, n > 0 ? info.ssi_signo : 0);
  close(fd);
  struct timespec invalid = {0, 1000000000};
  sigset_t none;
  sigemptyset(&none);
  int pr = ppoll(NULL, 0, &invalid, &none);
  printf("ppoll-invalid-timeout=%d errno=%d\n", pr, pr < 0 ? errno : 0);
  uint64_t zero = 0;
  row("restore", SIG_SETMASK, &zero, NULL, 8);
  printf("pending-end=%#llx\n", (unsigned long long)pending_now());
  return 0;
}

// A refusal row: a vfork child signals its parent, which waits in vfork, a
// kernel-run call the runtime mask would hold the signal through (D44).
static int mode_vfork_refusal(void) {
  printf("ROW-STARTED\n");
  fflush(stdout);
  pid_t parent = getpid();
  long child = syscall(SYS_vfork);
  if (child == 0) {
    syscall(SYS_kill, parent, SIGTERM);
    syscall(SYS_exit, 0);
  }
  reap((pid_t)child);
  printf("ROW-REACHED-AFTER-REFUSAL\n");
  return 0;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc != 2) return 2;
  const char *mode = argv[1];
  if (!strcmp(mode, "wait4")) return mode_wait4();
  if (!strcmp(mode, "d37")) return mode_d37();
  if (!strcmp(mode, "r2")) return mode_r2();
  if (!strcmp(mode, "sigtimedwait")) return mode_sigtimedwait();
  if (!strcmp(mode, "maskcalls")) return mode_maskcalls();
  if (!strcmp(mode, "vfork-refusal")) return mode_vfork_refusal();
  return 3;
}
