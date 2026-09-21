/*
 * umask_mode: cross-backend parity for the process file-creation mask.
 *
 * The effective permission bits of a newly created file or directory are the
 * requested mode masked by the process umask (mode & ~umask). Because the
 * fixture sets the umask explicitly before each creation, the resulting mode is
 * a fixed function of the program, independent of the host's inherited umask,
 * real user, or filesystem. umask() also returns the previous mask, which the
 * fixture checks to confirm the value round-trips.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* Number of behavioral steps this fixture must complete; a lower count is a
   failure, not a smaller success. */
#define EXPECTED_CHECKS 5

/* Return the low permission bits (st_mode & 07777) for a path, or -1. */
static int perm_bits(const char *path) {
  struct stat st;
  if (stat(path, &st) != 0) {
    return -1;
  }
  return (int)(st.st_mode & 07777);
}

int main(void) {
  int ok = 0;

  char root[] = "/tmp/umaskXXXXXX";
#ifdef HERMIT_TEST_UMASK_SETUP_FAILURE
  errno = EACCES;
  char *created_root = NULL;
#else
  char *created_root = mkdtemp(root);
#endif
  if (created_root == NULL) {
    fprintf(stderr, "umask setup failed: mkdtemp: %s\n", strerror(errno));
    return 1;
  }

  char file_a[128];
  snprintf(file_a, sizeof file_a, "%s/a", root);
  char file_b[128];
  snprintf(file_b, sizeof file_b, "%s/b", root);
  char dir_d[128];
  snprintf(dir_d, sizeof dir_d, "%s/d", root);

  /* Establish a known mask; the previous value is discarded here. */
  (void)umask(022);

  /* 1: creating a 0666 file under umask 022 yields mode 0644. */
  int fd_a = open(file_a, O_CREAT | O_WRONLY, 0666);
  if (fd_a >= 0) {
    close(fd_a);
    if (perm_bits(file_a) == 0644) {
      ok++;
    }
  }

  /* 2: switching to umask 077 returns the previous mask (022). */
  mode_t prev_mask = umask(077);
  if (prev_mask == 022) {
    ok++;
  }

  /* 3: creating a 0666 file under umask 077 yields mode 0600. */
  int fd_b = open(file_b, O_CREAT | O_WRONLY, 0666);
  if (fd_b >= 0) {
    close(fd_b);
    if (perm_bits(file_b) == 0600) {
      ok++;
    }
  }

  /* 4: mkdir 0777 under umask 077 yields mode 0700. */
  if (mkdir(dir_d, 0777) == 0 && perm_bits(dir_d) == 0700) {
    ok++;
  }

  /* 5: restoring the mask reports the intervening value (077). */
  mode_t restored = umask(022);
  if (restored == 077) {
    ok++;
  }

  unlink(file_a);
  unlink(file_b);
  rmdir(dir_d);
  rmdir(root);

  printf("umask ok=%d\n", ok);

  /* Every step above must have succeeded. Without this check the guest exits 0
     no matter how many steps failed, so a real semantic regression only lowers
     the printed count -- and because both --verify runs lower it identically,
     bitwise parity still holds and the cell passes green. */
  if (ok != EXPECTED_CHECKS) {
    fprintf(stderr, "umask completed %d of %d steps\n", ok, EXPECTED_CHECKS);
    return 1;
  }
  return 0;
}
