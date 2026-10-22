// tests/c/sigsuspend_shared_mask_rewrite.c's scenario with the mask on a
// shared stack, just below its red zone: the parent calls the raw rt_sigsuspend
// with its stack pointer at the top of a MAP_SHARED region and its mask 144
// bytes below it, below the red zone, where Hermit used to place its private
// copy of the mask. A peer process rewrites that mask to block
// SIGCHLD after Hermit has read it and before the call runs, while a child's
// exit sends the parent the SIGCHLD the mask let through; the peer then sends
// SIGUSR1.
//
// Linux copies the mask when rt_sigsuspend starts. Under Hermit's schedule the
// rewrite lands after the call started, so the first call wakes for the
// child's SIGCHLD and the second for the peer's SIGUSR1: "sigchld=1
// sigusr1=1". Natively the rewrite may land first, and SIGCHLD then stays
// blocked (sigchld=0). Exits 0 for either, and for nothing else.
//
// The slot is below the red zone, so a signal handler's frame may overwrite
// it: the SIGCHLD handler that runs after the first call puts the top of its
// saved FPU state there, whose bytes depend on the CPU. The parent therefore
// stores the mask again before every later call, as the peer does, rather
// than sleeping under whatever the frame left (on one CPU that unblocked
// SIGCHLD, and the peer's own exit was caught too: "sigchld=2").
//
// For diagnosis, the guest records each rt_sigsuspend call with the mask
// slot's value just before it, and each signal with its si_code and sender
// (child, peer or other), and prints that trace on stderr when it exits 1.
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define SHARED_STACK_SIZE (64 * 1024)

struct shared {
  volatile int suspending;
  // The stack the parent's rt_sigsuspend runs on; its signal handlers run on
  // it too.
  char stack[SHARED_STACK_SIZE] __attribute__((aligned(16)));
};

static volatile sig_atomic_t sigchld_count;
static volatile sig_atomic_t sigusr1_count;

// The trace: 'S' an rt_sigsuspend call (value: the mask slot before it), 'C'
// a SIGCHLD and 'U' a SIGUSR1 (value: si_code, sender: si_pid).
#define TRACE_MAX 16
static struct {
  char kind;
  long value;
  pid_t sender;
} trace[TRACE_MAX];
static volatile sig_atomic_t trace_length;
static pid_t child_pid;
static pid_t peer_pid;

static void record(char kind, long value, pid_t sender) {
  int at = trace_length;
  if (at < TRACE_MAX) {
    trace[at].kind = kind;
    trace[at].value = value;
    trace[at].sender = sender;
    trace_length = at + 1;
  }
}

static void on_sigchld(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  sigchld_count++;
  record('C', info->si_code, info->si_pid);
}

static void on_sigusr1(int sig, siginfo_t* info, void* context) {
  (void)sig;
  (void)context;
  sigusr1_count++;
  record('U', info->si_code, info->si_pid);
}

static const char* sender_name(pid_t sender) {
  if (sender == child_pid)
    return "child";
  if (sender == peer_pid)
    return "peer";
  return "other";
}

static void print_trace(FILE* out) {
  fprintf(out, "trace:");
  for (int i = 0; i < trace_length && i < TRACE_MAX; i++) {
    if (trace[i].kind == 'S')
      fprintf(out, " sigsuspend(mask=%#lx)", trace[i].value);
    else
      fprintf(
          out,
          " %s(code=%ld,from=%s)",
          trace[i].kind == 'C' ? "SIGCHLD" : "SIGUSR1",
          trace[i].value,
          sender_name(trace[i].sender));
  }
  fprintf(out, "\n");
}

// The mask's offset below the shared stack's top.
#define MASK_OFFSET 144
// The mask the peer stores, and the parent stores before every later call.
#define BLOCK_SIGCHLD (1ULL << (SIGCHLD - 1))

// rt_sigsuspend(top - offset, 8) with the stack pointer at `top`, and the
// stack pointer restored afterwards.
static long sigsuspend_on(char* top, long offset) {
  long result;
  __asm__ volatile(
      "mov %%rsp, %%r12\n\t"
      "mov %[top], %%rsp\n\t"
      "mov %%rsp, %%rdi\n\t"
      "sub %[offset], %%rdi\n\t"
      "mov $8, %%esi\n\t"
      "mov %[nr], %%eax\n\t"
      "syscall\n\t"
      "mov %%r12, %%rsp\n\t"
      : "=a"(result)
      : [top] "r"(top), [offset] "r"(offset), [nr] "i"(SYS_rt_sigsuspend)
      : "rdi", "rsi", "rcx", "r11", "r12", "memory");
  return result;
}

static int reap(pid_t pid) {
  int status;
  if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child %d did not exit cleanly\n", (int)pid);
    return 1;
  }
  return 0;
}

int main(void) {
  long offset = MASK_OFFSET;
  struct shared* shared = mmap(
      NULL,
      sizeof(*shared),
      PROT_READ | PROT_WRITE,
      MAP_SHARED | MAP_ANONYMOUS,
      -1,
      0);
  if (shared == MAP_FAILED) {
    perror("mmap");
    return 1;
  }
  char* top = shared->stack + SHARED_STACK_SIZE;
  volatile uint64_t* mask = (volatile uint64_t*)(top - offset);
  struct sigaction action;
  memset(&action, 0, sizeof(action));
  action.sa_sigaction = on_sigchld;
  action.sa_flags = SA_SIGINFO;
  if (sigaction(SIGCHLD, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  action.sa_sigaction = on_sigusr1;
  if (sigaction(SIGUSR1, &action, NULL) != 0) {
    perror("sigaction");
    return 1;
  }
  sigset_t all;
  sigfillset(&all);
  sigprocmask(SIG_SETMASK, &all, NULL);
  pid_t parent = getpid();

  pid_t peer = fork();
  if (peer < 0) {
    perror("fork");
    return 1;
  }
  peer_pid = peer;
  if (peer == 0) {
    while (!shared->suspending)
      sched_yield();
    sched_yield();
    *mask = BLOCK_SIGCHLD;
    for (int i = 0; i < 3; i++)
      sched_yield();
    kill(parent, SIGUSR1);
    _exit(0);
  }
  pid_t child = fork();
  if (child < 0) {
    perror("fork");
    return 1;
  }
  child_pid = child;
  if (child == 0) {
    while (!shared->suspending)
      sched_yield();
    _exit(0);
  }

  // A mask that blocks nothing, at the shared slot the peer rewrites.
  *mask = 0;
  shared->suspending = 1;
  sched_yield();
  for (int call = 0; !sigusr1_count; call++) {
    if (call > 0)
      *mask = BLOCK_SIGCHLD;
    record('S', (long)*mask, 0);
    long result = sigsuspend_on(top, offset);
    if (result != -4) {
      fprintf(stderr, "rt_sigsuspend returned %ld\n", result);
      return 1;
    }
  }
  if (reap(peer) || reap(child))
    return 1;
  printf("sigchld=%d sigusr1=%d\n", (int)sigchld_count, (int)sigusr1_count);
  if (sigchld_count <= 1 && sigusr1_count == 1)
    return 0;
  print_trace(stderr);
  return 1;
}
