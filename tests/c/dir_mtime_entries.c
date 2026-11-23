/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// A directory's mtime moves to the current time when a name is created in
// it, removed from it, or renamed into or out of it, and a new object's mtime
// is its creation time, as on Linux (mm/shmem.c `shmem_mknod`, `shmem_link`,
// `shmem_unlink`, `shmem_rmdir`, `shmem_symlink`; fs/libfs.c
// `simple_rename_timestamp`). Writing to a file that already has its name, a
// failed call, an `O_TMPFILE` open and a rename between two names of one file
// (fs/namei.c `vfs_rename`) change no directory
// (https://github.com/facebookexperimental/hermit/issues/433).
//
// Each directory is first set to an old mtime, so a directory a call should
// move and did not keeps a value that cannot be "now". A moved mtime must lie
// between the coarse clock read just before the call (the clock Linux stamps
// with) and the clock read just after it. The other test directory, and the
// working directory, which no call changes, must keep the old mtime. Each
// system call is also made through syscall(2), from spellings that tell its
// arguments apart, whichever call a C library's wrapper makes.

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

// 2001-09-09T01:46:40Z.
#define OLD 1000000000L

static int failures = 0;

static void die(const char* what) {
  perror(what);
  exit(2);
}

static struct timespec now(clockid_t clock) {
  struct timespec ts;
  if (clock_gettime(clock, &ts) != 0) {
    die("clock_gettime");
  }
  return ts;
}

static int before(struct timespec a, struct timespec b) {
  return a.tv_sec < b.tv_sec || (a.tv_sec == b.tv_sec && a.tv_nsec <= b.tv_nsec);
}

static struct timespec lmtime(const char* path) {
  struct stat st;
  if (lstat(path, &st) != 0) {
    die(path);
  }
  return st.st_mtim;
}

static void set_old(const char* path) {
  struct timespec times[2] = {{0, UTIME_OMIT}, {OLD, 0}};
  if (utimensat(AT_FDCWD, path, times, AT_SYMLINK_NOFOLLOW) != 0) {
    die("utimensat");
  }
}

static void report(const char* check, int ok, const char* path, struct timespec got) {
  if (ok) {
    printf("ok %s\n", check);
  } else {
    fprintf(
        stderr,
        "FAIL %s: %s has mtime %ld.%09ld\n",
        check,
        path,
        (long)got.tv_sec,
        (long)got.tv_nsec);
    failures++;
  }
}

// The window a call ran in: the coarse clock just before it, the fine clock
// just after it.
struct window {
  struct timespec start;
  struct timespec end;
};

static struct window open_window(void) {
  struct window w = {now(CLOCK_REALTIME_COARSE), {0, 0}};
  return w;
}

static void close_window(struct window* w) {
  w->end = now(CLOCK_REALTIME);
}

static int within(struct window w, struct timespec t) {
  return before(w.start, t) && before(t, w.end);
}

static void expect_moved(const char* check, const char* path, struct window w) {
  struct timespec got = lmtime(path);
  report(check, within(w, got), path, got);
}

static void expect_kept(const char* check, const char* path) {
  struct timespec got = lmtime(path);
  report(check, got.tv_sec == OLD && got.tv_nsec == 0, path, got);
}

// Sets both test directories to the old mtime, then opens the window of the
// call that follows.
static struct window begin(void) {
  set_old("d");
  set_old("e");
  return open_window();
}

// After a call that changed names in `directory` alone: it moved, and the
// other test directory kept the old mtime `begin` gave it.
static void expect_only(const char* check, const char* directory, struct window w) {
  const char* other = strcmp(directory, "d") == 0 ? "e" : "d";
  char kept[128];
  snprintf(kept, sizeof(kept), "%s-keeps-%s", check, other);
  expect_moved(check, directory, w);
  expect_kept(kept, other);
}

// A new object is stamped no later than its directory, which Linux stamps
// after it creates the inode.
static void expect_not_after(const char* check, const char* object, const char* directory) {
  struct timespec made = lmtime(object);
  report(check, before(made, lmtime(directory)), object, made);
}

static void write_new(const char* path) {
  int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0644);
  if (fd < 0 || write(fd, "x", 1) != 1 || close(fd) != 0) {
    die(path);
  }
}

int main(void) {
  const char* tmp = getenv("TMPDIR");
  char root[4096];
  snprintf(root, sizeof(root), "%s/dir-mtime-XXXXXX", tmp && *tmp ? tmp : "/tmp");
  if (mkdtemp(root) == NULL || chdir(root) != 0) {
    die("mkdtemp");
  }
  if (mkdir("d", 0755) != 0 || mkdir("e", 0755) != 0) {
    die("mkdir");
  }
  set_old(".");
  struct window w;

  // O_CREAT of an absent name creates it; of a present one does not.
  // Checked before the file is written: a write moves its mtime again.
  w = begin();
  int fd = open("d/x", O_WRONLY | O_CREAT, 0644);
  close_window(&w);
  if (fd < 0) {
    die("d/x");
  }
  expect_only("create", "d", w);
  expect_moved("create-new-file", "d/x", w);
  expect_not_after("create-file-not-after-directory", "d/x", "d");
  if (write(fd, "x", 1) != 1 || close(fd) != 0) {
    die("d/x");
  }

  set_old("d");
  fd = open("d/x", O_WRONLY | O_CREAT | O_APPEND, 0644);
  if (fd < 0 || write(fd, "y", 1) != 1 || close(fd) != 0) {
    die("d/x append");
  }
  expect_kept("append-existing", "d");

#ifdef SYS_creat
  w = begin();
  fd = (int)syscall(SYS_creat, "d/c", 0644);
  close_window(&w);
  if (fd < 0 || close(fd) != 0) {
    die("creat");
  }
  expect_only("creat", "d", w);
  expect_moved("creat-new-file", "d/c", w);
#endif

  // Renames: within a directory, across two, over an existing name, and
  // between two names of one file, which renames nothing.
  w = begin();
  if (rename("d/x", "d/y") != 0) {
    die("rename");
  }
  close_window(&w);
  expect_only("rename", "d", w);

  w = begin();
  if (rename("d/y", "e/y") != 0) {
    die("rename across");
  }
  close_window(&w);
  expect_moved("rename-across-old", "d", w);
  expect_moved("rename-across-new", "e", w);

  write_new("e/z");
  w = begin();
  if (rename("e/z", "e/y") != 0) {
    die("rename over");
  }
  close_window(&w);
  expect_only("rename-over", "e", w);

  if (link("e/y", "e/hard") != 0) {
    die("link");
  }
  set_old("e");
  if (rename("e/y", "e/hard") != 0) {
    die("rename onto own link");
  }
  expect_kept("rename-same-file", "e");

  write_new("d/swap");
  w = begin();
  if (syscall(SYS_renameat2, AT_FDCWD, "d/swap", AT_FDCWD, "e/y", RENAME_EXCHANGE) != 0) {
    die("renameat2 exchange");
  }
  close_window(&w);
  expect_moved("exchange-old", "d", w);
  expect_moved("exchange-new", "e", w);

  // A name added to an existing file changes its new directory, not the
  // file's own mtime.
  set_old("e/hard");
  w = begin();
  if (link("e/hard", "d/linked") != 0) {
    die("link");
  }
  close_window(&w);
  expect_only("link", "d", w);
  expect_kept("link-keeps-file", "d/linked");

  w = begin();
  if (unlink("d/linked") != 0) {
    die("unlink");
  }
  close_window(&w);
  expect_only("unlink", "d", w);

  // New directories, symbolic links and nodes read their creation time.
  w = begin();
  if (mkdir("d/sub/", 0755) != 0) {
    die("mkdir");
  }
  close_window(&w);
  expect_only("mkdir", "d", w);
  expect_moved("mkdir-new-directory", "d/sub", w);
  expect_not_after("mkdir-directory-not-after-parent", "d/sub", "d");

  w = begin();
  if (rmdir("d/sub") != 0) {
    die("rmdir");
  }
  close_window(&w);
  expect_only("rmdir", "d", w);

  w = begin();
  if (symlink("swap", "d/sym") != 0) {
    die("symlink");
  }
  close_window(&w);
  expect_only("symlink", "d", w);
  expect_moved("symlink-new-link", "d/sym", w);

  w = begin();
  if (mknod("d/fifo", S_IFIFO | 0644, 0) != 0) {
    die("mknod");
  }
  close_window(&w);
  expect_only("mknod", "d", w);
  expect_moved("mknod-new-fifo", "d/fifo", w);

  // The *at forms, from a directory descriptor.
  int dirfd = open("e", O_RDONLY | O_DIRECTORY);
  if (dirfd < 0) {
    die("open e");
  }
  w = begin();
  if (mkdirat(dirfd, "at", 0755) != 0) {
    die("mkdirat");
  }
  close_window(&w);
  expect_only("mkdirat", "e", w);
  w = begin();
  if (unlinkat(dirfd, "at", AT_REMOVEDIR) != 0) {
    die("unlinkat");
  }
  close_window(&w);
  expect_only("unlinkat-removedir", "e", w);
  w = begin();
  fd = openat(dirfd, "atfile", O_WRONLY | O_CREAT, 0644);
  close_window(&w);
  if (fd < 0 || close(fd) != 0) {
    die("openat");
  }
  expect_only("openat-create", "e", w);
  expect_moved("openat-new-file", "e/atfile", w);

  // The same calls made through syscall(2). A call that read its directory
  // from the wrong argument would move the other test directory, the working
  // directory, or none: a link's target and a call's other name are spelled
  // in the other directory, and a rename's two names from two descriptors.
  int ddir = open("d", O_RDONLY | O_DIRECTORY);
  if (ddir < 0) {
    die("open d");
  }
#ifdef SYS_rename
  w = begin();
  fd = (int)syscall(SYS_open, "d/o", O_WRONLY | O_CREAT, 0644);
  close_window(&w);
  if (fd < 0 || close(fd) != 0) {
    die("SYS_open");
  }
  expect_only("open-raw", "d", w);
  expect_moved("open-raw-new-file", "d/o", w);

  w = begin();
  if (syscall(SYS_mkdir, "e/m", 0755) != 0) {
    die("SYS_mkdir");
  }
  close_window(&w);
  expect_only("mkdir-raw", "e", w);
  expect_moved("mkdir-raw-new-directory", "e/m", w);

  w = begin();
  if (syscall(SYS_mknod, "e/f", S_IFIFO | 0644, 0) != 0) {
    die("SYS_mknod");
  }
  close_window(&w);
  expect_only("mknod-raw", "e", w);
  expect_moved("mknod-raw-new-fifo", "e/f", w);

  w = begin();
  if (syscall(SYS_symlink, "d/o", "e/s") != 0) {
    die("SYS_symlink");
  }
  close_window(&w);
  expect_only("symlink-raw", "e", w);
  expect_moved("symlink-raw-new-link", "e/s", w);

  w = begin();
  if (syscall(SYS_link, "d/o", "e/l") != 0) {
    die("SYS_link");
  }
  close_window(&w);
  expect_only("link-raw", "e", w);

  w = begin();
  if (syscall(SYS_unlink, "e/l") != 0) {
    die("SYS_unlink");
  }
  close_window(&w);
  expect_only("unlink-raw", "e", w);

  // A renamed object keeps its own mtime.
  set_old("e/f");
  w = begin();
  if (syscall(SYS_rename, "e/f", "d/f") != 0) {
    die("SYS_rename");
  }
  close_window(&w);
  expect_moved("rename-raw-old", "e", w);
  expect_moved("rename-raw-new", "d", w);
  expect_kept("rename-raw-keeps-object", "d/f");

  w = begin();
  if (syscall(SYS_rmdir, "e/m") != 0) {
    die("SYS_rmdir");
  }
  close_window(&w);
  expect_only("rmdir-raw", "e", w);
#endif

  w = begin();
  if (syscall(SYS_mknodat, ddir, "af", S_IFIFO | 0644, 0) != 0) {
    die("SYS_mknodat");
  }
  close_window(&w);
  expect_only("mknodat", "d", w);
  expect_moved("mknodat-new-fifo", "d/af", w);

  w = begin();
  if (syscall(SYS_symlinkat, "e/target", ddir, "as") != 0) {
    die("SYS_symlinkat");
  }
  close_window(&w);
  expect_only("symlinkat", "d", w);
  expect_moved("symlinkat-new-link", "d/as", w);

  w = begin();
  if (syscall(SYS_linkat, dirfd, "atfile", ddir, "al", 0) != 0) {
    die("SYS_linkat");
  }
  close_window(&w);
  expect_only("linkat", "d", w);

  w = begin();
  if (syscall(SYS_unlinkat, ddir, "al", 0) != 0) {
    die("SYS_unlinkat");
  }
  close_window(&w);
  expect_only("unlinkat-file", "d", w);

  // A moved directory keeps its own mtime too.
  if (mkdir("d/ad", 0755) != 0) {
    die("mkdir d/ad");
  }
  set_old("d/ad");
  w = begin();
  if (syscall(SYS_renameat, ddir, "ad", AT_FDCWD, "e/ad") != 0) {
    die("SYS_renameat");
  }
  close_window(&w);
  expect_moved("renameat-old", "d", w);
  expect_moved("renameat-new", "e", w);
  expect_kept("renameat-keeps-directory", "e/ad");

  w = begin();
  if (syscall(SYS_renameat2, ddir, "af", AT_FDCWD, "e/af", 0) != 0) {
    die("SYS_renameat2");
  }
  close_window(&w);
  expect_moved("renameat2-old", "d", w);
  expect_moved("renameat2-new", "e", w);

  w = begin();
  if (syscall(SYS_renameat2, AT_FDCWD, "e/af", ddir, "af2", RENAME_NOREPLACE) != 0) {
    die("SYS_renameat2 RENAME_NOREPLACE");
  }
  close_window(&w);
  expect_moved("renameat2-noreplace-old", "e", w);
  expect_moved("renameat2-noreplace-new", "d", w);

  // An exchange between two names of one file exchanges nothing.
  if (link("e/atfile", "d/same") != 0) {
    die("link d/same");
  }
  set_old("d");
  set_old("e");
  if (syscall(SYS_renameat2, AT_FDCWD, "d/same", AT_FDCWD, "e/atfile", RENAME_EXCHANGE) != 0) {
    die("SYS_renameat2 RENAME_EXCHANGE of one file");
  }
  expect_kept("exchange-same-file-old", "d");
  expect_kept("exchange-same-file-new", "e");

  // A name spelled from the root.
  char absolute[4200];
  snprintf(absolute, sizeof(absolute), "%s/e/abs", root);
  w = begin();
  if (mkdir(absolute, 0755) != 0) {
    die("mkdir absolute");
  }
  close_window(&w);
  expect_only("mkdir-absolute", "e", w);

  // An O_TMPFILE file has no name, so its directory keeps its mtime until a
  // link gives it one.
  w = begin();
  fd = open("d", O_TMPFILE | O_WRONLY, 0644);
  close_window(&w);
  if (fd < 0) {
    die("O_TMPFILE");
  }
  expect_kept("tmpfile", "d");
  struct stat st;
  if (fstat(fd, &st) != 0) {
    die("fstat");
  }
  report("tmpfile-new-file", within(w, st.st_mtim), "O_TMPFILE file", st.st_mtim);
  w = open_window();
  int linked = linkat(fd, "", AT_FDCWD, "d/named", AT_EMPTY_PATH);
  if (linked != 0) {
    char proc[64];
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", fd);
    linked = linkat(AT_FDCWD, proc, AT_FDCWD, "d/named", AT_SYMLINK_FOLLOW);
  }
  close_window(&w);
  if (linked != 0 || close(fd) != 0) {
    die("linkat O_TMPFILE");
  }
  expect_only("tmpfile-link", "d", w);

  // Failed calls change nothing.
  set_old("d");
  if (mkdir("d/named", 0755) == 0 || errno != EEXIST) {
    die("mkdir over a file");
  }
  if (unlink("d/absent") == 0 || errno != ENOENT) {
    die("unlink of an absent name");
  }
  if (open("d/named", O_WRONLY | O_CREAT | O_EXCL, 0644) >= 0 || errno != EEXIST) {
    die("O_EXCL over a file");
  }
  if (syscall(SYS_renameat2, AT_FDCWD, "d/named", AT_FDCWD, "d/swap", RENAME_NOREPLACE) == 0 ||
      errno != EEXIST) {
    die("RENAME_NOREPLACE over a file");
  }
  expect_kept("failed-calls", "d");
  expect_kept("working-directory-kept", ".");

  // Clean up, so a native run leaves nothing behind.
  const char* names[] = {
      "d/c",
      "d/swap",
      "d/sym",
      "d/fifo",
      "d/named",
      "d/o",
      "d/f",
      "d/as",
      "d/af2",
      "d/same",
      "e/y",
      "e/hard",
      "e/atfile",
      "e/s"};
  for (size_t i = 0; i < sizeof(names) / sizeof(names[0]); i++) {
    if (unlink(names[i]) != 0 && errno != ENOENT) {
      die(names[i]);
    }
  }
  if (rmdir("e/ad") != 0 || rmdir(absolute) != 0) {
    die("cleanup of directories");
  }
  if (close(ddir) != 0 || close(dirfd) != 0 || rmdir("d") != 0 || rmdir("e") != 0 ||
      chdir("/") != 0 || rmdir(root) != 0) {
    die("cleanup");
  }
  if (failures) {
    fprintf(stderr, "%d checks failed\n", failures);
    return 1;
  }
  printf("directory mtimes follow entries\n");
  return 0;
}
