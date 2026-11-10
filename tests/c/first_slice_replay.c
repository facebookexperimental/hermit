// A new thread that computes before its first syscall.
//
// The main thread starts a thread with clone(2), which runs the loop below
// (about 3000 conditional branches) and then exits by a raw exit syscall, so
// the new thread enters Detcore for the first time at its first timer
// preemption, not at a syscall. The main thread waits for it on the futex the
// kernel clears at its exit, then prints the result, which is the same in
// every run.
//
// Under `--chaos` with a short `--max-timeslice` the recording ends the new
// thread's first slice at a timer, after fewer branches than the maximum. A
// schedule replay must end that first slice where the recording did; a
// replay that gave the thread a full slice instead would run past it and
// desynchronize every later event.
#define _GNU_SOURCE
#include <linux/futex.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

#define STACK_SIZE (256 * 1024)
#define ITERATIONS 1500

static volatile uint64_t result;
// The kernel clears this and wakes its futex when the new thread exits
// (CLONE_CHILD_CLEARTID); it is nonzero until then.
static volatile pid_t running = 1;

static int compute(void* arg) {
  uint64_t x = (uint64_t)(uintptr_t)arg;
  for (unsigned i = 0; i < ITERATIONS; i++) {
    if (x & 1) {
      x = 3 * x + 1;
    } else {
      x >>= 1;
    }
    x += i;
  }
  result = x;
  // Exit this thread alone, by the raw syscall: no library code runs in the
  // new thread before it.
  syscall(SYS_exit, 0);
  return 0;
}

int main(void) {
  char* stack = malloc(STACK_SIZE);
  if (stack == NULL) {
    perror("malloc");
    return 1;
  }
  pid_t tid;
  int flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND |
      CLONE_THREAD | CLONE_SYSVSEM | CLONE_PARENT_SETTID |
      CLONE_CHILD_CLEARTID;
  if (clone(compute, stack + STACK_SIZE, flags, (void*)(uintptr_t)27, &tid,
            NULL, &running) == -1) {
    perror("clone");
    return 1;
  }
  for (pid_t seen = running; seen != 0; seen = running) {
    syscall(SYS_futex, &running, FUTEX_WAIT, seen, NULL, NULL, 0);
  }
  printf("result=%llu\n", (unsigned long long)result);
  return 0;
}
