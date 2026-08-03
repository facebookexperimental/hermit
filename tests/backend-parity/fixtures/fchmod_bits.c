// Backend-parity fixture: file permission-bit round-trip.
//
// Creates a private temporary file and drives its permission bits through
// fchmod(2), chmod(2), and fchmodat(2), reading the result back with
// fstat(2)/stat(2) each time. The mode a program sets on a file it owns is a
// deterministic property of the guest filesystem view, independent of host
// umask or the host's real inode metadata, so ptrace, DBI, and KVM must all
// observe the same permission bits. Only the low twelve mode bits are examined
// and only the final self-set mode is printed; no host-derived ownership,
// device, inode, or timestamp field is exposed.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py).
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

// Return the permission bits (low 12) currently reported for an open fd, or -1.
static long fd_mode(int fd) {
  struct stat st;
  if (fstat(fd, &st) != 0) {
    return -1;
  }
  return (long)(st.st_mode & 07777);
}

// Return the permission bits (low 12) currently reported for a path, or -1.
static long path_mode(const char *path) {
  struct stat st;
  if (stat(path, &st) != 0) {
    return -1;
  }
  return (long)(st.st_mode & 07777);
}

int main(void) {
  char path[] = "/tmp/fchmod_bits_XXXXXX";
  int fd = mkstemp(path);
  if (fd < 0) {
    perror("mkstemp");
    return 1;
  }

  int ok = 0;

  // fchmod on the open descriptor, observed through the same descriptor.
  if (fchmod(fd, 0640) == 0 && fd_mode(fd) == 0640) {
    ok++;
  }

  // A second fchmod fully replaces the bits rather than OR-ing them.
  if (fchmod(fd, 0600) == 0 && fd_mode(fd) == 0600) {
    ok++;
  }

  // chmod by path is observed through a fresh stat by path.
  if (chmod(path, 0644) == 0 && path_mode(path) == 0644) {
    ok++;
  }

  // fchmodat with AT_FDCWD resolves the same path and replaces the bits.
  if (fchmodat(AT_FDCWD, path, 0640, 0) == 0 && path_mode(path) == 0640) {
    ok++;
  }

  // The descriptor opened before the path-based changes still sees them:
  // fd and path name the same inode, so the final mode agrees on both.
  long final_fd = fd_mode(fd);
  long final_path = path_mode(path);
  if (final_fd == final_path && final_fd == 0640) {
    ok++;
  }

  long mode = final_path;
  if (close(fd) != 0) {
    ok = -1;
  }
  if (unlink(path) != 0) {
    ok = -1;
  }

  printf("fchmod_bits mode=%ld ok=%d\n", mode, ok);
  return 0;
}
