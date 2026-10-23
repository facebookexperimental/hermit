/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// Descriptor syncs, metadata changes, xattr calls and zero-length reads
// whose results must come from the recording. The first file lives in a host
// directory absent from the replay chroot, so replay hands the guest a
// placeholder descriptor: a live fsync, fchmod or fgetxattr there answers
// EINVAL or EOPNOTSUPP instead of the recorded result. The second file exists
// in the replay root, where descriptor and path xattr calls must agree. Every
// result is checked against its expected value, so record and replay cannot
// agree on a wrong failure and still pass.

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/xattr.h>
#include <unistd.h>

#define NAME "user.hermit"
#define VALUE "recorded"
// Set on the host directory by the test harness before recording.
#define PRE "user.pre"

// An access ACL: ACL_USER_OBJ rwx, ACL_GROUP_OBJ and ACL_OTHER nothing.
static const struct {
  uint32_t version;
  struct {
    uint16_t tag;
    uint16_t perm;
    uint32_t id;
  } entries[3];
} acl = {2, {{0x01, 7, (uint32_t)-1}, {0x04, 0, (uint32_t)-1}, {0x20, 0, (uint32_t)-1}}};

static int failures;

// Print a result and require it to succeed (expected_errno == 0) or to fail
// with expected_errno. A call with one correct success value uses
// expect_value instead, so any other nonnegative result fails.
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

static void expect_value(const char* name, long result, long wanted) {
  expect(name, result, 0);
  if (result >= 0 && result != wanted) {
    printf("UNEXPECTED %s: wanted %ld\n", name, wanted);
    failures++;
  }
}

// A sized query must return the value the size query promised, and the
// buffer must hold the recorded bytes.
static void expect_xattr_value(const char* name, long result, const char* buf) {
  expect_value(name, result, (long)strlen(VALUE));
  if (result == (long)strlen(VALUE) && memcmp(buf, VALUE, result) != 0) {
    printf("UNEXPECTED %s: value %.*s\n", name, (int)result, buf);
    failures++;
  }
}

// A list must hold NAME as one of its NUL-separated entries. Other entries,
// such as a security label, depend on the host and are not checked.
static void expect_list(const char* name, long result, const char* buf) {
  expect(name, result, 0);
  int found = 0;
  for (long i = 0; i < result; i += strlen(buf + i) + 1) {
    found |= strcmp(buf + i, NAME) == 0;
  }
  if (!found) {
    printf("UNEXPECTED %s: no %s entry\n", name, NAME);
    failures++;
  }
}

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s HOST_DIRECTORY\n", argv[0]);
    return 2;
  }
  char path[4096];
  snprintf(path, sizeof(path), "%s/file", argv[1]);
  int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
  if (fd < 0) {
    perror("open");
    return 2;
  }
  char buf[256];

  // A read that transfers nothing never touches the buffer, so NULL is valid.
  expect_value("read-null-zero", syscall(SYS_read, fd, NULL, 0), 0);
  expect_value("read-zero", syscall(SYS_read, fd, buf, 0), 0);
  expect_value("pread-null-zero", syscall(SYS_pread64, fd, NULL, 0, 0), 0);
  expect_value("read-eof-null", syscall(SYS_read, fd, NULL, 16), 0);

  expect_value("fsync", fsync(fd), 0);
  expect_value("fdatasync", fdatasync(fd), 0);
  expect_value("syncfs", syncfs(fd), 0);
  expect_value("fchmod", fchmod(fd, 0600), 0);
  expect_value("fchown", fchown(fd, -1, -1), 0);

  expect_value("fsetxattr", fsetxattr(fd, NAME, VALUE, strlen(VALUE), 0), 0);
  // A zero size asks only for the length; the buffer is not touched.
  expect_value(
      "fgetxattr-size", fgetxattr(fd, NAME, NULL, 0), (long)strlen(VALUE));
  memset(buf, 0, sizeof(buf));
  expect_xattr_value(
      "fgetxattr", fgetxattr(fd, NAME, buf, sizeof(buf)), buf);
  memset(buf, 0, sizeof(buf));
  expect_xattr_value("getxattr", getxattr(path, NAME, buf, sizeof(buf)), buf);
  memset(buf, 0, sizeof(buf));
  expect_xattr_value(
      "lgetxattr", lgetxattr(path, NAME, buf, sizeof(buf)), buf);
  expect("fgetxattr-small", fgetxattr(fd, NAME, buf, 1), ERANGE);
  expect("fgetxattr-missing", fgetxattr(fd, "user.absent", buf, 8), ENODATA);

  long size = flistxattr(fd, NULL, 0);
  expect("flistxattr-size", size, 0);
  memset(buf, 0, sizeof(buf));
  long listed = flistxattr(fd, buf, sizeof(buf));
  expect_list("flistxattr", listed, buf);
  if (listed != size) {
    printf(
        "UNEXPECTED flistxattr: %ld bytes, size query said %ld\n",
        listed,
        size);
    failures++;
  }
  memset(buf, 0, sizeof(buf));
  expect_list("listxattr", listxattr(path, buf, sizeof(buf)), buf);
  memset(buf, 0, sizeof(buf));
  expect_list("llistxattr", llistxattr(path, buf, sizeof(buf)), buf);

  expect_value("fremovexattr", fremovexattr(fd, NAME), 0);
  expect("fgetxattr-removed", fgetxattr(fd, NAME, buf, sizeof(buf)), ENODATA);

  struct stat st;
  expect_value("fstat", fstat(fd, &st), 0);
  if ((st.st_mode & 07777) != 0600) {
    printf("UNEXPECTED fstat mode %o\n", st.st_mode & 07777);
    failures++;
  }
  // Attributes the kernel consults on later calls, set through the
  // placeholder: strip copies its input's ACL to its output this way, and an
  // install step sets a capability on a staged binary. The replay root lacks
  // the file, so no live call can reach either.
  expect_value(
      "fsetxattr-acl-placeholder",
      fsetxattr(fd, "system.posix_acl_access", &acl, sizeof(acl), 0),
      0);
  struct vfs_cap_data cap = {VFS_CAP_REVISION_2, {{1u << CAP_NET_RAW, 0}, {0, 0}}};
  expect_value(
      "fsetxattr-cap-placeholder",
      fsetxattr(fd, "security.capability", &cap, XATTR_CAPS_SZ_2, 0),
      0);
  expect_value("close", close(fd), 0);

  // The working directory and a file the guest creates in it exist in the
  // replay root too (replay enters the recorded directory), so descriptor and
  // path calls mix there. The replay root is built without the host's
  // attributes: the directory's user.pre, which the test harness set before
  // recording, is absent from it, yet replacing, removing and re-creating it
  // must replay what the recording saw. No live call reads a user attribute
  // back (queries replay from the recording), so the replay root may lack it.
  if (chdir(argv[1]) != 0) {
    perror("chdir");
    return 2;
  }
  int dir = open(".", O_RDONLY | O_DIRECTORY);
  expect("open-dir", dir, 0);
  expect_value(
      "dir-setxattr-pre-replace", setxattr(".", PRE, "p", 1, XATTR_REPLACE), 0);
  expect_value("dir-fremovexattr-pre", fremovexattr(dir, PRE), 0);
  expect("dir-fremovexattr-pre-gone", fremovexattr(dir, PRE), ENODATA);
  expect_value(
      "dir-setxattr-pre-create", setxattr(".", PRE, "p", 1, XATTR_CREATE), 0);
  expect_value("dir-removexattr-pre", removexattr(".", PRE), 0);
  expect_value("close-dir", close(dir), 0);
  int local = open("local", O_CREAT | O_RDWR | O_TRUNC, 0644);
  expect("open-local", local, 0);
  expect_value(
      "local-fsetxattr", fsetxattr(local, NAME, VALUE, strlen(VALUE), 0), 0);
  expect_value("local-removexattr", removexattr("local", NAME), 0);
  expect("local-fremovexattr-gone", fremovexattr(local, NAME), ENODATA);
  expect_value("local-fsetxattr-again", fsetxattr(local, NAME, "v2", 2, 0), 0);
  expect(
      "local-setxattr-create",
      setxattr("local", NAME, VALUE, strlen(VALUE), XATTR_CREATE),
      EEXIST);
  expect_value(
      "local-lsetxattr-replace",
      lsetxattr("local", NAME, VALUE, strlen(VALUE), XATTR_REPLACE),
      0);
  memset(buf, 0, sizeof(buf));
  expect_xattr_value(
      "local-fgetxattr", fgetxattr(local, NAME, buf, sizeof(buf)), buf);
  expect_value("local-lremovexattr", lremovexattr("local", NAME), 0);
  expect("local-fremovexattr-after", fremovexattr(local, NAME), ENODATA);
  expect_value("local-fchmod", fchmod(local, 0600), 0);
  expect_value("close-local", close(local), 0);

  // An access ACL rewrites the mode bits, which govern path calls replay runs
  // live in the replay root. With its capabilities dropped, the guest can
  // rename inside a mode-0 directory only because the ACL granted the owner
  // rwx, so replay must carry each ACL into the replay root.
  expect_value("mkdir-acl", mkdir("acl", 0700), 0);
  expect_value("mkdir-acl-a", mkdir("acl/a", 0700), 0);
  expect_value("mkdir-facl", mkdir("facl", 0700), 0);
  expect_value("mkdir-facl-a", mkdir("facl/a", 0700), 0);
  int facl = open("facl", O_RDONLY | O_DIRECTORY);
  expect("open-facl", facl, 0);
  struct __user_cap_header_struct cap_header = {_LINUX_CAPABILITY_VERSION_3, 0};
  struct __user_cap_data_struct no_caps[2];
  memset(no_caps, 0, sizeof(no_caps));
  expect_value("capset-none", syscall(SYS_capset, &cap_header, no_caps), 0);
  expect_value("chmod-acl-0", chmod("acl", 0), 0);
  expect_value("chmod-facl-0", chmod("facl", 0), 0);
  expect_value(
      "setxattr-acl",
      setxattr("acl", "system.posix_acl_access", &acl, sizeof(acl), 0),
      0);
  expect_value(
      "fsetxattr-acl",
      fsetxattr(facl, "system.posix_acl_access", &acl, sizeof(acl), 0),
      0);
  expect_value("rename-acl", rename("acl/a", "acl/b"), 0);
  expect_value("rename-facl", rename("facl/a", "facl/b"), 0);
  expect_value("close-facl", close(facl), 0);

  printf("failures=%d\n", failures);
  return failures ? 1 : 0;
}
