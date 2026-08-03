// Backend-parity fixture: rename(2)/renameat(2) semantics.
//
// Drives file renames within a private mkdtemp root and confirms the standard
// POSIX outcomes: an intra-directory rename moves the content and removes the
// source name; renaming onto an existing destination atomically replaces it;
// renameat across two directory descriptors relocates the entry; and renaming a
// nonexistent source fails with ENOENT. Only presence/absence and file size are
// observed, all derived from the guest's own writes, so the result is a
// deterministic property of the filesystem operations rather than of host inode
// metadata or timing. ptrace, DBI, and KVM must agree.
//
// _GNU_SOURCE is supplied by the harness compile flags (see run_matrix.py).
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

enum { ROOT_CAP = 64, PATH_CAP = 128 };

// Create a file at path holding exactly data; 0 on success, -1 otherwise.
static int mkfile(const char *path, const char *data) {
  int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (fd < 0) {
    return -1;
  }
  size_t len = strlen(data);
  ssize_t written = write(fd, data, len);
  close(fd);
  return written == (ssize_t)len ? 0 : -1;
}

// 1 if stat(path) fails with ENOENT (the name is absent), 0 otherwise.
static int missing(const char *path) {
  struct stat st;
  return stat(path, &st) != 0 && errno == ENOENT;
}

// File size at path, or -1 if it cannot be stat'd.
static long fsize(const char *path) {
  struct stat st;
  return stat(path, &st) == 0 ? (long)st.st_size : -1;
}

int main(void) {
  char root[ROOT_CAP] = "/tmp/rename_ops_XXXXXX";
  if (mkdtemp(root) == NULL) {
    perror("mkdtemp");
    return 1;
  }

  int ok = 0;
  char a[PATH_CAP];
  char b[PATH_CAP];
  char sub[PATH_CAP];
  char subb[PATH_CAP];
  char src[PATH_CAP];
  char dst[PATH_CAP];
  snprintf(a, sizeof a, "%s/a", root);
  snprintf(b, sizeof b, "%s/b", root);
  snprintf(sub, sizeof sub, "%s/sub", root);

  // Intra-directory rename: a -> b. The source name disappears and b holds the
  // 5-byte payload.
  if (mkfile(a, "hello") == 0 && rename(a, b) == 0 && missing(a) &&
      fsize(b) == 5) {
    ok++;
  }

  // Rename onto an existing destination atomically replaces it: b now holds the
  // new 6-byte payload, and the source name is again gone.
  if (mkfile(a, "worldx") == 0 && rename(a, b) == 0 && missing(a) &&
      fsize(b) == 6) {
    ok++;
  }

  // renameat across directory descriptors: move root/b to sub/c. b disappears
  // from the root directory and the content survives under its new name.
  if (mkdir(sub, 0755) == 0) {
    int rootfd = open(root, O_RDONLY | O_DIRECTORY);
    int subfd = open(sub, O_RDONLY | O_DIRECTORY);
    if (rootfd >= 0 && subfd >= 0 &&
        renameat(rootfd, "b", subfd, "c") == 0) {
      snprintf(subb, sizeof subb, "%s/sub/c", root);
      if (missing(b) && fsize(subb) == 6) {
        ok++;
      }
    }
    if (rootfd >= 0) {
      close(rootfd);
    }
    if (subfd >= 0) {
      close(subfd);
    }
  }

  // Renaming a nonexistent source fails deterministically with ENOENT.
  snprintf(src, sizeof src, "%s/nope", root);
  snprintf(dst, sizeof dst, "%s/dst", root);
  if (rename(src, dst) != 0 && errno == ENOENT) {
    ok++;
  }

  // Remove the whole temporary tree so the fixture is idempotent. Leaving the
  // mkdtemp root behind would make a second run (for example the second pass of
  // hermit --verify, which replays the same deterministic random stream) collide
  // on the same candidate name and retry, perturbing the syscall count.
  snprintf(subb, sizeof subb, "%s/sub/c", root);
  unlink(subb);
  rmdir(sub);
  rmdir(root);

  printf("rename_ops ok=%d\n", ok);
  return 0;
}
