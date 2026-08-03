/*
 * fsync_durability: cross-backend parity for file durability syscalls.
 *
 * fsync, fdatasync, and syncfs have no observable data effect in a fixture, but
 * their return values are deterministic: success on a valid writable descriptor
 * and EBADF on a closed one. The contract records those outcomes so a backend
 * cannot silently turn a durability barrier into an error.
 *
 * ptrace and DBI forward all three. The KVM ElfExecutor personality forwards
 * fsync and fdatasync but returns deterministic ENOSYS for syncfs, so KVM is a
 * documented gap on this row (ok=5 instead of ok=6).
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(void) {
  int ok = 0;

  char path[] = "/tmp/fsyncXXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    printf("sync ok=0\n");
    return 0;
  }

  /* 1: a short write establishes dirty data to flush. */
  if (write(fd, "hello", 5) == 5) {
    ok++;
  }

  /* 2: fsync succeeds on the valid writable descriptor. */
  if (fsync(fd) == 0) {
    ok++;
  }

  /* 3: fdatasync succeeds on the same descriptor. */
  if (fdatasync(fd) == 0) {
    ok++;
  }

  /* 4: syncfs succeeds on the descriptor's filesystem. */
  if (syncfs(fd) == 0) {
    ok++;
  }

  /* 5: fsync on a bad descriptor fails deterministically with EBADF. */
  if (fsync(-1) == -1 && errno == EBADF) {
    ok++;
  }

  /* 6: fdatasync on a bad descriptor fails deterministically with EBADF. */
  if (fdatasync(-1) == -1 && errno == EBADF) {
    ok++;
  }

  close(fd);
  unlink(path);

  printf("sync ok=%d\n", ok);
  return 0;
}
