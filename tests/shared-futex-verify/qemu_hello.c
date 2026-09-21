/*
 * Minimal statically linked glibc userspace program for the QEMU-under-Hermit
 * "run a real userspace program" test. Prints a deterministic marker plus its
 * (virtualized) pid and exits with a fixed nonzero status so the launcher init
 * can prove it captured the exact exit code across two --verify runs.
 *
 * Deliberately avoids getaddrinfo / NSS / dlopen (the static-NSS trap) and any
 * wall-clock or environment-dependent output, so its stdout is bitwise-stable.
 */
#include <stdio.h>
#include <unistd.h>

int main(void) {
  printf("QEMU_USERSPACE_HELLO_OK pid=%d\n", (int)getpid());
  fflush(stdout);
  return 7;
}
