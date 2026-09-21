/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Regression guest for the determinized `chown` family: issue #1849,
 * implemented by PR #1851 (https://github.com/rrnewton/hermit/pull/1851).
 *
 * Detcore fixes the guest identity at root, so an ownership change must report
 * the answer a real root gets -- success, for any uid -- instead of the errno
 * of whatever host identity the backend happens to run under. But root
 * privilege waives only the ownership PERMISSION check. It does not waive
 * pathname, descriptor, or flag errors: a real root's
 * `chown("/does/not/exist", 0, 0)` still fails with ENOENT.
 *
 * This guest asserts BOTH halves, because each one alone is satisfied by a
 * defect:
 *
 *   - an unconditional `Ok(0)` satisfies the permission half and turns every
 *     error case into a silent success;
 *   - a plain pass-through satisfies the error half and reintroduces the
 *     host-dependent EPERM/EINVAL the determinization exists to remove.
 *
 * Only an implementation that emulates the identity half, validates the
 * argument half, and still applies the metadata consequence Linux attaches to
 * a successful chown -- clearing set-id bits and moving ctime -- passes all of
 * the checks below. Skipping that last part is its own defect: hermit clears
 * the bits today under pass-through, so a determinization that stops doing it
 * would be a privilege-containment regression.
 *
 * Success is printing "chown-virtual-root-identity-ok" and exiting 0. Every
 * failure prints the exact call, what was expected, and what was observed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

/* A uid that is deliberately NOT the caller and NOT root: under a one-uid user
 * namespace this is the value that used to fail with EINVAL, and with no user
 * namespace at all the whole family used to fail with EPERM. */
#define FOREIGN_UID 1000
#define FOREIGN_GID 1000

static int failures = 0;

static const char* errno_name(int err) {
  const char* name = strerrorname_np(err);
  return name ? name : "?";
}

/* The identity half: a virtual root's ownership change must SUCCEED. */
static void expect_ok(const char* what, int rc) {
  if (rc == 0) {
    printf("ok       %-42s rc=0\n", what);
    return;
  }
  printf("FAIL     %-42s expected rc=0, got rc=%d errno=%s\n", what, rc, errno_name(errno));
  failures++;
}

/* The argument half: an error that has nothing to do with identity must still
 * reach the guest, with the errno Linux would have produced. */
static void expect_errno(const char* what, int rc, int wanted) {
  if (rc == 0) {
    printf("FAIL     %-42s expected %s, got rc=0 (error swallowed)\n", what, errno_name(wanted));
    failures++;
    return;
  }
  if (errno != wanted) {
    printf("FAIL     %-42s expected %s, got %s\n", what, errno_name(wanted), errno_name(errno));
    failures++;
    return;
  }
  printf("ok       %-42s rc=-1 errno=%s\n", what, errno_name(wanted));
}

int main(void) {
  const char* path = "chown_virtual_root_target";
  const char* link_path = "chown_virtual_root_symlink";
  const char* missing = "chown_virtual_root_no_such_file";

  unlink(path);
  unlink(link_path);

  int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  if (symlink(path, link_path) != 0) {
    perror("symlink");
    return 1;
  }

  /* Arm the privilege bits Linux strips on a successful chown. chown_common()
   * sets ATTR_KILL_SUID|ATTR_KILL_SGID for every non-directory and ATTR_CTIME
   * unconditionally, so this happens even when both requested ids are -1. A
   * determinized chown that skips setattr entirely would leave a setuid binary
   * setuid where the kernel disarms it, so the guest asserts the kernel's
   * outcome, not the implementation's convenience.
   *
   * 06755 is chosen because it exercises both bits at once and the file is
   * group-executable, which is the condition Linux requires before it clears
   * S_ISGID. The non-group-executable case is checked separately below. */
  if (fchmod(fd, 06755) != 0) {
    perror("fchmod(set-id canary)");
    return 1;
  }
  struct stat before;
  if (fstat(fd, &before) != 0) {
    perror("fstat(before)");
    return 1;
  }
  const mode_t mode_before = before.st_mode & 07777;
  if (mode_before != 06755) {
    printf("FAIL     %-42s expected mode=6755, got mode=%04o\n", "set-id canary armed",
           mode_before);
    return 1;
  }

  /* ---- Identity half: these all failed before #1849. ---- */
  expect_ok("chown(file, FOREIGN)", chown(path, FOREIGN_UID, FOREIGN_GID));
  expect_ok("chown(file, 0, 0)", chown(path, 0, 0));
  expect_ok("fchown(fd, FOREIGN)", fchown(fd, FOREIGN_UID, FOREIGN_GID));
  expect_ok("lchown(symlink, FOREIGN)", lchown(link_path, FOREIGN_UID, FOREIGN_GID));
  expect_ok("fchownat(AT_FDCWD, file, FOREIGN)",
            (int)syscall(SYS_fchownat, AT_FDCWD, path, FOREIGN_UID, FOREIGN_GID, 0));
  /* -1 means "leave unchanged"; a real root gets 0 and so must the guest. */
  expect_ok("chown(file, -1, -1)", chown(path, (uid_t)-1, (gid_t)-1));

  /* ---- Argument half: none of these is an identity error, so a real root
   * still receives every one of them. An unconditional success swallows the
   * lot -- that is the regression this half exists to catch. ---- */
  errno = 0;
  expect_errno("chown(MISSING)", chown(missing, 0, 0), ENOENT);
  errno = 0;
  expect_errno("lchown(MISSING)", lchown(missing, 0, 0), ENOENT);
  errno = 0;
  expect_errno("fchownat(AT_FDCWD, MISSING)",
               (int)syscall(SYS_fchownat, AT_FDCWD, missing, 0, 0, 0), ENOENT);
  errno = 0;
  expect_errno("fchown(-1)", fchown(-1, 0, 0), EBADF);
  errno = 0;
  expect_errno("fchown(9999)", fchown(9999, 0, 0), EBADF);
  errno = 0;
  /* O_PATH is valid for fstat but not fchown. A validator that substitutes
   * fstat without checking the descriptor kind silently swallows this EBADF. */
  int path_fd = open(path, O_PATH | O_CLOEXEC);
  if (path_fd < 0) {
    perror("open(O_PATH)");
    return 1;
  }
  expect_errno("fchown(O_PATH fd)", fchown(path_fd, 0, 0), EBADF);
  close(path_fd);
  errno = 0;
  /* A regular file used as a directory component. */
  expect_errno("chown(file/child)", chown("chown_virtual_root_target/child", 0, 0), ENOTDIR);
  errno = 0;
  /* An unrecognised AT_* flag is rejected before anything else happens. */
  expect_errno("fchownat(bad flags)",
               (int)syscall(SYS_fchownat, AT_FDCWD, path, 0, 0, 0x4000), EINVAL);

  /* ---- Read-back. Two different expectations, deliberately:
   *
   * OWNERSHIP is untouched, because the identity half is emulated rather than
   * forwarded. Detcore does not model per-file ownership, so the read-back
   * shows the unchanged owner even though the calls above reported success.
   * That divergence is the documented boundary; assert it so a future change
   * that starts really mutating ownership cannot pass silently.
   *
   * The METADATA CONSEQUENCE, by contrast, must match the kernel exactly: a
   * successful chown clears set-id bits and moves ctime, so those are asserted
   * to have HAPPENED, not to have been skipped. ---- */
  struct stat st;
  if (stat(path, &st) != 0) {
    perror("stat");
    return 1;
  }
  if (st.st_uid != before.st_uid || st.st_gid != before.st_gid) {
    printf("FAIL     %-42s owner changed %u:%u -> %u:%u\n", "stat(file) read-back",
           (unsigned)before.st_uid, (unsigned)before.st_gid, (unsigned)st.st_uid,
           (unsigned)st.st_gid);
    failures++;
  } else {
    printf("ok       %-42s owner unchanged (%u:%u), as documented\n", "stat(file) read-back",
           (unsigned)st.st_uid, (unsigned)st.st_gid);
  }
  /* Set-id clearing is a privilege-containment mechanism, not bookkeeping, and
   * Linux has applied it to root like any other user since 2.2.13. Measured
   * natively: 06755 -> 0755 for chown(path, -1, -1). */
  const mode_t mode_after = st.st_mode & 07777;
  if (mode_after != 0755) {
    printf("FAIL     %-42s expected set-id cleared %04o -> 0755, got %04o\n",
           "stat(file) mode read-back", mode_before, mode_after);
    failures++;
  } else {
    printf("ok       %-42s set-id cleared %04o -> %04o, as Linux does\n",
           "stat(file) mode read-back", mode_before, mode_after);
  }
  /* ATTR_CTIME is set unconditionally, so ctime moves even when there was
   * nothing to clear and even for a directory. */
  if (st.st_ctim.tv_sec == before.st_ctim.tv_sec && st.st_ctim.tv_nsec == before.st_ctim.tv_nsec) {
    printf("FAIL     %-42s ctime did not move (%lld.%09ld); Linux always sets ATTR_CTIME\n",
           "stat(file) ctime read-back", (long long)st.st_ctim.tv_sec, st.st_ctim.tv_nsec);
    failures++;
  } else {
    printf("ok       %-42s ctime moved %lld.%09ld -> %lld.%09ld\n", "stat(file) ctime read-back",
           (long long)before.st_ctim.tv_sec, before.st_ctim.tv_nsec, (long long)st.st_ctim.tv_sec,
           st.st_ctim.tv_nsec);
  }

  /* The condition on S_ISGID, which a "clear both bits always" implementation
   * gets wrong: Linux clears it only when the file is also group-executable.
   * Measured natively: 02644 stays 02644, while 02755 becomes 0755. */
  {
    const char* sgid_path = "chown_virtual_root_sgid_no_xgrp";
    int sgid_fd = open(sgid_path, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (sgid_fd < 0) {
      perror("open(sgid canary)");
      return 1;
    }
    if (fchmod(sgid_fd, 02644) != 0) {
      perror("fchmod(sgid canary)");
      return 1;
    }
    expect_ok("chown(sgid, no x-grp, -1, -1)", chown(sgid_path, (uid_t)-1, (gid_t)-1));
    struct stat sgid_st;
    if (stat(sgid_path, &sgid_st) != 0) {
      perror("stat(sgid canary)");
      return 1;
    }
    const mode_t sgid_mode = sgid_st.st_mode & 07777;
    if (sgid_mode != 02644) {
      printf("FAIL     %-42s S_ISGID must survive without S_IXGRP: 2644 -> %04o\n",
             "stat(sgid no-x-grp) mode read-back", sgid_mode);
      failures++;
    } else {
      printf("ok       %-42s S_ISGID preserved (%04o), no S_IXGRP\n",
             "stat(sgid no-x-grp) mode read-back", sgid_mode);
    }
    close(sgid_fd);
    unlink(sgid_path);
  }

  /* Directories are exempt from the clearing entirely (the !S_ISDIR guard in
   * chown_common). Measured natively: a directory at 06755 stays 06755. */
  {
    const char* dir_path = "chown_virtual_root_dir";
    if (mkdir(dir_path, 0755) != 0 && errno != EEXIST) {
      perror("mkdir(dir canary)");
      return 1;
    }
    if (chmod(dir_path, 06755) != 0) {
      perror("chmod(dir canary)");
      return 1;
    }
    expect_ok("chown(directory, -1, -1)", chown(dir_path, (uid_t)-1, (gid_t)-1));
    struct stat dir_st;
    if (stat(dir_path, &dir_st) != 0) {
      perror("stat(dir canary)");
      return 1;
    }
    const mode_t dir_mode = dir_st.st_mode & 07777;
    if (dir_mode != 06755) {
      printf("FAIL     %-42s directories are exempt: 6755 -> %04o\n",
             "stat(directory) mode read-back", dir_mode);
      failures++;
    } else {
      printf("ok       %-42s set-id preserved on directory (%04o)\n",
             "stat(directory) mode read-back", dir_mode);
    }
    rmdir(dir_path);
  }

  close(fd);
  unlink(link_path);
  unlink(path);

  if (failures != 0) {
    printf("chown-virtual-root-identity-FAILED %d\n", failures);
    return 1;
  }
  printf("chown-virtual-root-identity-ok\n");
  return 0;
}
