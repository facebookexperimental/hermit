/*
 * mknod_special: cross-backend parity for special-file node creation.
 *
 * Exercises mkfifo / mknodat / mknod for FIFO and regular-file nodes, plus the
 * EEXIST refusal on a duplicate path. Every node is created and then inspected
 * with lstat only -- the FIFO is never opened, because opening a FIFO without a
 * peer blocks indefinitely and would livelock the single-threaded DBI backend.
 *
 * Character/block device nodes are deliberately omitted: creating them requires
 * privilege the strict guest does not hold, so their behavior is a permission
 * policy question rather than a syscall-parity contract.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* Return the raw st_mode for a path via lstat, or -1 on error. */
static int lmode(const char *path) {
  struct stat st;
  if (lstat(path, &st) != 0) {
    return -1;
  }
  return (int)st.st_mode;
}

int main(void) {
  int ok = 0;

  char root[] = "/tmp/mknodXXXXXX";
  if (mkdtemp(root) == NULL) {
    printf("mknod ok=0\n");
    return 0;
  }

  int root_fd = open(root, O_RDONLY | O_DIRECTORY);

  char fifo[128];
  snprintf(fifo, sizeof fifo, "%s/f", root);
  char fifo_at[128];
  snprintf(fifo_at, sizeof fifo_at, "%s/f2", root);
  char reg[128];
  snprintf(reg, sizeof reg, "%s/r", root);

  /* 1: mkfifo creates a FIFO node visible to lstat as S_ISFIFO. */
  if (mkfifo(fifo, 0640) == 0) {
    int m = lmode(fifo);
    if (m >= 0 && S_ISFIFO(m)) {
      ok++;
    }
  }

  /* 2: mknodat with S_IFIFO, relative to a directory fd, creates a FIFO. */
  if (root_fd >= 0 && mknodat(root_fd, "f2", S_IFIFO | 0600, 0) == 0) {
    int m = lmode(fifo_at);
    if (m >= 0 && S_ISFIFO(m)) {
      ok++;
    }
  }

  /* 3: mknod with S_IFREG creates an ordinary empty regular file. */
  if (mknod(reg, S_IFREG | 0644, 0) == 0) {
    int m = lmode(reg);
    if (m >= 0 && S_ISREG(m)) {
      ok++;
    }
  }

  /* 4: mknod on an existing path fails deterministically with EEXIST. */
  if (mknod(reg, S_IFREG | 0644, 0) == -1 && errno == EEXIST) {
    ok++;
  }

  unlink(fifo);
  unlink(fifo_at);
  unlink(reg);
  if (root_fd >= 0) {
    close(root_fd);
  }
  rmdir(root);

  printf("mknod ok=%d\n", ok);
  return 0;
}
