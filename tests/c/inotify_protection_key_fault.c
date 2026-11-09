/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// An inotify read into a page that a protection key keeps the guest from
// writing. Linux takes the event from the queue, faults at the buffer's first
// byte and copies nothing: copy_to_user honours the calling thread's key
// rights. The buffer holds the twelve bytes of an earlier event header,
// cookie included, before a read-only page, and they must stay exactly as
// they were. A copy that does not check protection keys (process_vm_writev,
// ptrace) could write them, so where the copy ended cannot be found that way.

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/mman.h>
#include <unistd.h>

static void report(const char *what, int ok, int *all) {
  printf("%s: %s\n", what, ok ? "yes" : "no");
  *all = *all && ok;
}

int main(void) {
  int all = 1;
  long page = sysconf(_SC_PAGESIZE);
  char dir[] = "/tmp/inotify-protection-key-XXXXXX";
  if (!mkdtemp(dir)) return 1;

  // One 16-byte IN_IGNORED event, queued by removing a watch.
  int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
  int watch = fd < 0 ? -1 : inotify_add_watch(fd, dir, IN_ATTRIB);
  if (watch < 0 || inotify_rm_watch(fd, watch) != 0) return 2;

  // Twelve bytes of an earlier event header (cookie 4242) at the end of a
  // page, before a read-only one. The guest writes them, then a protection
  // key takes away its right to write their page.
  char *map = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (map == MAP_FAILED || mprotect(map + page, page, PROT_READ) != 0) return 3;
  char *header = map + page - 12;
  struct inotify_event earlier = {1, IN_MOVED_FROM, 4242, 0};
  memcpy(header, &earlier, 12);
  int pkey = pkey_alloc(0, PKEY_DISABLE_WRITE);
  report("a protection key that denies writes is allocated", pkey >= 0, &all);
  if (pkey < 0) return 9;
  report("the buffer's page takes the key",
         pkey_mprotect(map, page, PROT_READ | PROT_WRITE, pkey) == 0, &all);

  int faulted = read(fd, header, 16) == -1 && errno == EFAULT;
  report("a read into the buffer fails with EFAULT", faulted, &all);
  report("the earlier event header is as it was", memcmp(header, &earlier, 12) == 0, &all);
  char rest[64];
  report("the event left the queue", read(fd, rest, sizeof rest) == -1 && errno == EAGAIN, &all);

  rmdir(dir);
  return all ? 0 : 9;
}
