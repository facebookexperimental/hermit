/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Path queries and legacy path mutations whose results must come from the
// recording. Replay runs in a chroot where host files such as /etc/passwd and
// directories such as /usr/bin are absent, so a live access, faccessat, chdir
// or getcwd answers differently there. Raw syscall numbers keep libc from
// substituting the *at forms. Every result is checked against its expected
// value, so record and replay cannot agree on a wrong failure and still pass.

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif

static int failures;

// Print a result and require it to succeed (expected_errno == 0) or to fail
// with expected_errno.
static void expect(const char* name, long result, int expected_errno) {
  int actual_errno = result < 0 ? errno : 0;
  if (result < 0) {
    printf("%s=-1 errno=%s\n", name, strerrorname_np(actual_errno));
  } else {
    printf("%s=%ld\n", name, result);
  }
  if (actual_errno != expected_errno) {
    printf(
        "UNEXPECTED %s: wanted %s\n",
        name,
        expected_errno ? strerrorname_np(expected_errno) : "success");
    failures++;
  }
}

static long create(const char* path) {
  int fd = open(path, O_CREAT | O_WRONLY | O_EXCL, 0644);
  if (fd >= 0) {
    close(fd);
  }
  return fd < 0 ? -1 : 0;
}

// Send fd over a socketpair with SCM_RIGHTS, close the original and return
// the received copy, or -1.
static int receive_passed_fd(int fd) {
  int pair[2];
  if (fd < 0 || socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0) {
    return -1;
  }
  char byte = 'x';
  struct iovec iov = {.iov_base = &byte, .iov_len = 1};
  union {
    struct cmsghdr header;
    char buffer[CMSG_SPACE(sizeof(int))];
  } control;
  memset(&control, 0, sizeof(control));
  struct msghdr message = {
      .msg_iov = &iov,
      .msg_iovlen = 1,
      .msg_control = control.buffer,
      .msg_controllen = sizeof(control.buffer)};
  struct cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
  cmsg->cmsg_level = SOL_SOCKET;
  cmsg->cmsg_type = SCM_RIGHTS;
  cmsg->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(cmsg), &fd, sizeof(int));
  int received = -1;
  if (sendmsg(pair[0], &message, 0) == 1) {
    memset(&control, 0, sizeof(control));
    message.msg_controllen = sizeof(control.buffer);
    if (recvmsg(pair[1], &message, 0) == 1 &&
        (cmsg = CMSG_FIRSTHDR(&message)) != NULL &&
        cmsg->cmsg_type == SCM_RIGHTS) {
      memcpy(&received, CMSG_DATA(cmsg), sizeof(int));
    }
  }
  close(fd);
  close(pair[0]);
  close(pair[1]);
  return received;
}

// Enter a directory and remove it, then fchdir back into it. The fchdir
// succeeds on Linux, but the resulting working directory has no name; the
// recorder must refuse rather than record procfs's "<dir> (deleted)" text.
static int removed_cwd(void) {
  char dir[] = "/tmp/hermit-rr-removed-cwd.XXXXXX";
  if (mkdtemp(dir) == NULL) {
    perror("mkdtemp");
    return 1;
  }
  int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
  expect("chdir_removed", syscall(SYS_chdir, dir), 0);
  expect("rmdir_removed", syscall(SYS_rmdir, dir), 0);
  expect("fchdir_removed", syscall(SYS_fchdir, dirfd), 0);
  printf("removed-cwd-recorded failures=%d\n", failures);
  return 0;
}

// The directories under the argument exist on the host but not in the replay
// chroot. Replay must still move its working directory there, or both rounds
// of relative mutations land in one stale directory and the second link
// collides with the first.
static void mutate_in(const char* base, const char* name) {
  char dir[4096];
  snprintf(dir, sizeof(dir), "%s/%s", base, name);
  expect("chdir_host_dir", syscall(SYS_chdir, dir), 0);
  expect("create_f", create("f"), 0);
  expect("link_f_g", syscall(SYS_link, "f", "g"), 0);
}

static void host_directory_rounds(const char* base) {
  char path[4096];
  mutate_in(base, "first");
  mutate_in(base, "second");

  // fchdir back into the first host directory: its "f" must be reachable
  // relative to the restored working directory.
  snprintf(path, sizeof(path), "%s/first", base);
  // Pass the directory descriptor through SCM_RIGHTS: replay reproduces the
  // received descriptor as a placeholder, not a directory in the replay root,
  // so replay cannot fchdir through the descriptor itself.
  int dirfd = receive_passed_fd(open(path, O_RDONLY | O_DIRECTORY));
  expect("passed_host_dir", dirfd < 0 ? -1 : 0, 0);
  // Leave from second after giving it an "x" linked to "y": a replayed fchdir
  // that left the working directory there would collide on first's own link.
  snprintf(path, sizeof(path), "%s/second", base);
  expect("chdir_second_before_fchdir", syscall(SYS_chdir, path), 0);
  expect("create_x_in_second", create("x"), 0);
  expect("link_x_y_in_second", syscall(SYS_link, "x", "y"), 0);
  expect("fchdir_host_dir", syscall(SYS_fchdir, dirfd), 0);
  expect("link_f_h", syscall(SYS_link, "f", "h"), 0);
  expect("create_x_in_first", create("x"), 0);
  expect("link_x_y_in_first", syscall(SYS_link, "x", "y"), 0);
  if (dirfd >= 0) {
    close(dirfd);
  }

  // "via" is a host symlink to first/sub, so "via/.." is first, not base.
  // Replay has no such symlink; a replayed chdir that followed the spelling
  // would land in base, and base's own link to "q" below would then collide.
  snprintf(path, sizeof(path), "%s/via/..", base);
  expect("chdir_via_symlink_parent", syscall(SYS_chdir, path), 0);
  expect("create_p_in_first", create("p"), 0);
  expect("link_p_q_in_first", syscall(SYS_link, "p", "q"), 0);
  expect("chdir_base", syscall(SYS_chdir, base), 0);
  expect("create_p_in_base", create("p"), 0);
  expect("link_p_q_in_base", syscall(SYS_link, "p", "q"), 0);

  // A working directory longer than the replayer's injection buffer, entered
  // through the short host symlink "longvia" to "long/d.../d..." (six
  // 100-byte components). Neither exists in the replay chroot, so only a
  // replay that enters the recorded resolved directory succeeds.
  char component[101];
  memset(component, 'd', 100);
  component[100] = '\0';
  char deep[4096];
  snprintf(deep, sizeof(deep), "%s/long", base);
  for (int i = 0; i < 6; i++) {
    size_t used = strlen(deep);
    snprintf(deep + used, sizeof(deep) - used, "/%s", component);
  }
  snprintf(path, sizeof(path), "%s/longvia", base);
  expect("chdir_longvia", syscall(SYS_chdir, path), 0);
  char cwd[4096];
  long length = syscall(SYS_getcwd, cwd, sizeof(cwd));
  expect("getcwd_deep", length, 0);
  if (length <= 512 || strcmp(cwd, deep) != 0) {
    printf("UNEXPECTED getcwd_deep: length %ld\n", length);
    failures++;
  }
  expect("create_f_deep", create("f"), 0);
  expect("link_f_g_deep", syscall(SYS_link, "f", "g"), 0);
}

int main(int argc, char** argv) {
  if (argc > 1 && strcmp(argv[1], "--removed-cwd") == 0) {
    return removed_cwd();
  }
  expect("access_passwd_r", syscall(SYS_access, "/etc/passwd", R_OK), 0);
  expect(
      "faccessat_passwd_r",
      syscall(SYS_faccessat, AT_FDCWD, "/etc/passwd", R_OK),
      0);
  expect(
      "faccessat2_usr_bin_x",
      syscall(SYS_faccessat2, AT_FDCWD, "/usr/bin", X_OK, AT_EACCESS),
      0);
  expect(
      "faccessat_missing",
      syscall(SYS_faccessat, AT_FDCWD, "/nonexistent/hermit", F_OK),
      ENOENT);

  char cwd[4096];
  // /usr/bin, not /usr/lib: the pinned validation root has no /usr/lib.
  expect("chdir_usr_bin", syscall(SYS_chdir, "/usr/bin"), 0);
  long length = syscall(SYS_getcwd, cwd, sizeof(cwd));
  expect("getcwd", length, 0);
  if (length > 0) {
    printf("cwd=%s\n", cwd);
    if (cwd[0] != '/' || (size_t)length != strlen(cwd) + 1) {
      printf("UNEXPECTED getcwd: length %ld does not match the path\n", length);
      failures++;
    }
  }
  expect("getcwd_small", syscall(SYS_getcwd, cwd, 2), ERANGE);
  expect(
      "chdir_missing", syscall(SYS_chdir, "/nonexistent/hermit"), ENOENT);

  if (argc > 1) {
    host_directory_rounds(argv[1]);
  }

  char dir[] = "/tmp/hermit-rr-paths.XXXXXX";
  if (mkdtemp(dir) == NULL) {
    perror("mkdtemp");
    return 1;
  }
  expect("chdir_tmp", syscall(SYS_chdir, dir), 0);
  expect("create_a", create("a"), 0);
  expect("chmod", syscall(SYS_chmod, "a", 0600), 0);
  expect("rename", syscall(SYS_rename, "a", "b"), 0);
  expect("link", syscall(SYS_link, "b", "c"), 0);
  expect("symlink", syscall(SYS_symlink, "b", "d"), 0);
  expect("chown", syscall(SYS_chown, "b", -1, -1), 0);
  expect("lchown", syscall(SYS_lchown, "d", -1, -1), 0);
  expect("mknod", syscall(SYS_mknod, "p", S_IFIFO | 0600, 0), 0);
  expect("mkdir", mkdir("e", 0755), 0);
  expect("rmdir", syscall(SYS_rmdir, "e"), 0);
  expect("rename_missing", syscall(SYS_rename, "missing", "x"), ENOENT);

  struct stat st = {0};
  expect("stat_b", stat("b", &st), 0);
  printf("b_mode=%o b_nlink=%lu\n", st.st_mode & 07777, st.st_nlink);
  if ((st.st_mode & 07777) != 0600 || st.st_nlink != 2) {
    printf("UNEXPECTED stat_b: wanted mode 600 and 2 links\n");
    failures++;
  }

  unlink("b");
  unlink("c");
  unlink("d");
  unlink("p");
  expect("chdir_root", syscall(SYS_chdir, "/"), 0);
  expect("rmdir_tmp", syscall(SYS_rmdir, dir), 0);
  printf("failures=%d\n", failures);
  return failures == 0 ? 0 : 1;
}
