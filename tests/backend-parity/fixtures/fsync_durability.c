/*
 * fsync_durability: cross-backend parity for file durability syscalls.
 *
 * fsync, fdatasync, and syncfs have no observable data effect in a fixture, but
 * their return values are deterministic: success on a valid writable descriptor
 * and EBADF on a closed one. The contract records those outcomes so a backend
 * cannot silently turn a durability barrier into an error.
 *
 * ptrace and DBT forward all three. The KVM ElfExecutor personality forwards
 * fsync and fdatasync but returns deterministic ENOSYS for syncfs, so KVM is a
 * documented gap on this row (ok=5 instead of ok=6).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

/* Number of behavioural checks this fixture must complete; a lower count is a
   failure, not a smaller success.

   CONSEQUENCE TO WEIGH BEFORE ENABLING ANOTHER CELL: the KVM ElfExecutor
   personality returns deterministic ENOSYS for syncfs and so reaches 5, as this
   file's own header records. The only enabled cell is verify/ptrace, where 6 is
   correct. Enabling verify/kvm would make this fixture exit 1 until that gap is
   closed -- which is the honest report, not a reason to lower the constant.
*/
#define EXPECTED_CHECKS 6

int main(void) {
  int ok = 0;

  char path[] = "/tmp/fsyncXXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    printf("sync ok=0\n");
    return 1;
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
  /* Route a behavioural failure into the exit status. Without this the guest
       exits 0 whatever `ok` reached, so a regression only lowered the printed
       number -- and under --verify both runs lower it identically, so the
       comparison still matches and the cell stays green. Every check above is
       unchanged; this only requires all of them. */
    if (ok != EXPECTED_CHECKS) {
        fprintf(stderr, "sync completed %d of %d checks\n", ok, EXPECTED_CHECKS);
        return 1;
    }
    return 0;
}
