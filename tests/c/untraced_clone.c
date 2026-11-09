/* clone with CLONE_UNTRACED, in the shapes a program uses it, for
 * hermit-cli/tests/cli.rs.
 *
 *   helper   the shape of LeakSanitizer's exit-time helper: a thread-like
 *            child sharing memory, files and fs, but not a thread
 *            (CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_UNTRACED, no exit
 *            signal), on its own stack; the parent waits for it with
 *            waitpid(__WALL).
 *   process  a fork-like child (CLONE_UNTRACED | SIGCHLD) that exits 7; the
 *            parent waits for it with waitpid.
 *   thread   a thread (CLONE_THREAD | CLONE_VM | CLONE_SIGHAND | ... |
 *            CLONE_UNTRACED) that stores a value and exits; the parent waits
 *            on CLONE_CHILD_CLEARTID's futex.
 *   clone3   the process shape through clone3.
 *
 * Each prints one line naming what it saw, then the guest exits 0.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <linux/sched.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define STACK_SIZE (256 << 10)

static volatile int child_value;
static volatile pid_t clear_tid = 1;

static int helper_child(void* arg) {
  (void)arg;
  child_value = 42;
  return 0;
}

static int thread_child(void* arg) {
  (void)arg;
  child_value = 43;
  syscall(SYS_exit, 0); /* this thread only */
  return 0;
}

static char* new_stack(void) {
  char* stack = mmap(NULL, STACK_SIZE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
  if (stack == MAP_FAILED) {
    perror("mmap");
    exit(3);
  }
  return stack + STACK_SIZE;
}

static int helper(void) {
  pid_t child = clone(helper_child, new_stack(),
                      CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_UNTRACED, NULL);
  if (child < 0) {
    printf("helper clone failed errno=%d\n", errno);
    return 0;
  }
  int status = 0;
  pid_t waited = waitpid(child, &status, __WALL);
  printf("helper waited=%d exited=%d code=%d value=%d\n", waited == child,
         WIFEXITED(status), WEXITSTATUS(status), child_value);
  return 0;
}

static int process(void) {
  pid_t child = (pid_t)syscall(SYS_clone, CLONE_UNTRACED | SIGCHLD, NULL, NULL,
                               NULL, NULL);
  if (child == 0) {
    _exit(7);
  }
  if (child < 0) {
    printf("process clone failed errno=%d\n", errno);
    return 0;
  }
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  printf("process waited=%d exited=%d code=%d\n", waited == child,
         WIFEXITED(status), WEXITSTATUS(status));
  return 0;
}

static int thread(void) {
  int flags = CLONE_THREAD | CLONE_VM | CLONE_SIGHAND | CLONE_FS | CLONE_FILES |
              CLONE_SYSVSEM | CLONE_CHILD_CLEARTID | CLONE_UNTRACED;
  pid_t child = clone(thread_child, new_stack(), flags, NULL, NULL, NULL,
                      (pid_t*)&clear_tid);
  if (child < 0) {
    printf("thread clone failed errno=%d\n", errno);
    return 0;
  }
  while (clear_tid != 0) {
    syscall(SYS_futex, &clear_tid, FUTEX_WAIT, clear_tid, NULL, NULL, 0);
  }
  printf("thread joined=1 value=%d\n", child_value);
  return 0;
}

static int process_clone3(void) {
  struct clone_args args;
  memset(&args, 0, sizeof(args));
  args.flags = CLONE_UNTRACED;
  args.exit_signal = SIGCHLD;
  pid_t child = (pid_t)syscall(SYS_clone3, &args, sizeof(args));
  if (child == 0) {
    _exit(7);
  }
  if (child < 0) {
    printf("clone3 failed errno=%d\n", errno);
    return 0;
  }
  int status = 0;
  pid_t waited = waitpid(child, &status, 0);
  printf("clone3 waited=%d exited=%d code=%d\n", waited == child,
         WIFEXITED(status), WEXITSTATUS(status));
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  const char* mode = argc == 2 ? argv[1] : "";
  if (strcmp(mode, "helper") == 0) {
    return helper();
  }
  if (strcmp(mode, "process") == 0) {
    return process();
  }
  if (strcmp(mode, "thread") == 0) {
    return thread();
  }
  if (strcmp(mode, "clone3") == 0) {
    return process_clone3();
  }
  fprintf(stderr, "usage: %s helper | process | thread | clone3\n", argv[0]);
  return 2;
}
