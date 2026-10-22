// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

// A volatile address prevents the compiler from replacing the null store with
// an explicit trap. This must be a CPU fault, not kill/tgkill/raise: those
// enter a different, Tool-controlled signal-delivery path.
static volatile uintptr_t fault_address;

__attribute__((noreturn, noinline)) static void null_dereference(void) {
  *(volatile unsigned char*)fault_address = 1;
  _exit(90); // Reaching here means the required SIGSEGV did not happen.
}

static int orphan_fault(void) {
  int hold[2];
  if (pipe(hold) != 0)
    return 91;
  pid_t middle = fork();
  if (middle < 0)
    return 92;
  if (middle == 0) {
    // Save this PID before forking. Reading getppid() for the first time in the
    // grandchild would race the middle's exit and could already return init.
    pid_t direct_parent = getpid();
    pid_t orphan = fork();
    if (orphan < 0)
      _exit(93);
    if (orphan == 0) {
      if (close(hold[0]) != 0)
        _exit(94);
      while (getppid() == direct_parent)
        syscall(SYS_sched_yield);
      // The direct parent is now terminal, so the backend must use its
      // DirectParentTerminal exit path. The live root is an independent reader.
      // Keep the last pipe writer open until the actual synchronous fault.
      null_dereference();
    }
    _exit(0);
  }

  if (close(hold[1]) != 0)
    return 95;
  int status = 0;
  pid_t waited;
  do {
    waited = waitpid(middle, &status, 0);
  } while (waited < 0 && errno == EINTR);
  if (waited != middle || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
    return 96;
  char byte;
  ssize_t count;
  do {
    count = read(hold[0], &byte, 1);
  } while (count < 0 && errno == EINTR);
  if (count != 0 || close(hold[0]) != 0)
    return 97;
  const char output[] = "root saw n=0\n";
  if (write(STDOUT_FILENO, output, sizeof(output) - 1) !=
      (ssize_t)(sizeof(output) - 1))
    return 98;
  return 0;
}

int main(int argc, char** argv) {
  if (argc != 2)
    return 99;
  if (strcmp(argv[1], "root") == 0)
    null_dereference();
  if (strcmp(argv[1], "orphan") == 0)
    return orphan_fault();
  return 99;
}
