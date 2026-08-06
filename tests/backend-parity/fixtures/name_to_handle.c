/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

/*
 * name_to_handle_at exports an opaque, filesystem-internal file handle whose
 * bytes encode host-specific inode and generation numbers. Hermit refuses the
 * call deterministically rather than leaking that non-portable state into the
 * guest. This contract asserts that every backend produces the same refusal:
 * the call fails with EOPNOTSUPP without populating a handle. EOPNOTSUPP is a
 * faithful Linux response -- the kernel returns it for filesystems that do not
 * support handle export -- so a guest observes only that handles are
 * unavailable, never a divergent host-derived value.
 */

#define HANDLE_CAP 128

union handle_storage {
  struct file_handle handle;
  unsigned char raw[sizeof(struct file_handle) + HANDLE_CAP];
};

int main(void) {
  int ok = 0;

  union handle_storage storage;
  memset(&storage, 0, sizeof(storage));
  storage.handle.handle_bytes = HANDLE_CAP;
  storage.handle.handle_type = 0x5a5a5a5a;
  int mount_id = 0x1234;

  errno = 0;
  int result =
      name_to_handle_at(AT_FDCWD, "/", &storage.handle, &mount_id, 0);
  if (result == -1 && errno == EOPNOTSUPP) {
    ok++;
  } else {
    fprintf(
        stderr,
        "name_to_handle_at(/) returned %d errno %d (%s), expected EOPNOTSUPP\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  /* The refusal must not have partially populated the caller's handle: the
   * sentinel type is preserved. */
  if (storage.handle.handle_type == 0x5a5a5a5a) {
    ok++;
  } else {
    fprintf(
        stderr,
        "handle_type mutated to %d on refusal\n",
        storage.handle.handle_type);
    return 1;
  }

  memset(&storage, 0, sizeof(storage));
  storage.handle.handle_bytes = HANDLE_CAP;
  storage.handle.handle_type = 0x5a5a5a5a;

  errno = 0;
  result = name_to_handle_at(AT_FDCWD, ".", &storage.handle, &mount_id, 0);
  if (result == -1 && errno == EOPNOTSUPP) {
    ok++;
  } else {
    fprintf(
        stderr,
        "name_to_handle_at(.) returned %d errno %d (%s), expected EOPNOTSUPP\n",
        result,
        errno,
        strerror(errno));
    return 1;
  }

  printf("name_to_handle ok=%d\n", ok);
  return 0;
}
