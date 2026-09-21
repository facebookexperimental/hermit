// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// THREE-WAY IDENTITY AGREEMENT.
//
// A guest can learn a file's device and inode through three routes:
//   stat(2)                  -- determinized by determinize_stat
//   /proc/self/maps          -- the mapping header's dev and inode columns
//   /proc/self/fdinfo/<fd>   -- the "ino:" field
// Detcore presents a sanitized world, so ALL THREE MUST AGREE. They did not:
// maps was not a recognized procfs kind, so it reported the raw host device
// and inode while stat reported determinized ones -- two identities for one
// file, visible to any guest that inspects itself.
//
// The test maps a memfd rather than the executable image. That distinction is
// load-bearing: on btrfs, maps and stat may legitimately report different
// device values for an executable on a subvolume. A memfd has one backing
// device through both routes, so both the device and inode columns must agree.
// This lets the guest catch a partial repair that rewrites only the inode while
// leaving the raw host device visible.
//
// Deliberately no dependence on the VALUES, only on their AGREEMENT: the
// determinized numbers are an implementation detail and pinning them would
// make this a change-detector instead of a consistency check.

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

static int fail(const char* what) {
  fprintf(stderr, "FAIL: %s\n", what);
  return 1;
}

// Find the mapping header covering `addr` and return its dev and inode.
static int maps_identity(uintptr_t addr, unsigned* major_out, unsigned* minor_out,
                         unsigned long* inode_out) {
  FILE* maps = fopen("/proc/self/maps", "r");
  if (!maps) {
    return -1;
  }
  char line[4096];
  while (fgets(line, sizeof(line), maps)) {
    uintptr_t start = 0, end = 0;
    unsigned major = 0, minor = 0;
    unsigned long inode = 0;
    // address perms offset dev inode pathname
    if (sscanf(line, "%lx-%lx %*4s %*x %x:%x %lu", &start, &end, &major, &minor,
               &inode) != 5) {
      continue;
    }
    if (addr >= start && addr < end) {
      *major_out = major;
      *minor_out = minor;
      *inode_out = inode;
      fclose(maps);
      return 0;
    }
  }
  fclose(maps);
  return -1;
}

// Read the "ino:" field from /proc/self/fdinfo/<fd>.
static int fdinfo_inode(int fd, unsigned long* inode_out) {
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  FILE* info = fopen(path, "r");
  if (!info) {
    return -1;
  }
  char line[512];
  while (fgets(line, sizeof(line), info)) {
    if (strncmp(line, "ino:", 4) == 0) {
      *inode_out = strtoul(line + 4, NULL, 10);
      fclose(info);
      return 0;
    }
  }
  fclose(info);
  return -1;
}

int main(void) {
  // A memfd has one kernel backing device through both maps and fstat, unlike
  // a path on a btrfs subvolume where Linux may legitimately report the
  // superblock device in maps and the subvolume device in stat.
  int fd = memfd_create("hermit-procfs-identity-agreement", MFD_CLOEXEC);
  if (fd < 0) {
    return fail("create mapped file");
  }
  if (ftruncate(fd, 4096) != 0) {
    return fail("ftruncate");
  }
  struct stat st;
  if (fstat(fd, &st) != 0) {
    return fail("fstat");
  }
  void* mapping = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
  if (mapping == MAP_FAILED) {
    return fail("mmap");
  }

  unsigned maps_major = 0, maps_minor = 0;
  unsigned long maps_ino = 0;
  if (maps_identity((uintptr_t)mapping, &maps_major, &maps_minor, &maps_ino) != 0) {
    return fail("no maps line covers the mapping");
  }

  int rc = 0;
  unsigned long stat_ino = (unsigned long)st.st_ino;
  unsigned stat_major = major(st.st_dev);
  unsigned stat_minor = minor(st.st_dev);

  if (maps_ino != stat_ino) {
    fprintf(stderr, "INODE DISAGREES: maps=%lu stat=%lu\n", maps_ino, stat_ino);
    rc = 1;
  }
  if (maps_major != stat_major || maps_minor != stat_minor) {
    fprintf(stderr, "DEVICE DISAGREES: maps=%x:%x stat=%x:%x\n",
            maps_major, maps_minor, stat_major, stat_minor);
    rc = 1;
  }

  unsigned long fdinfo_ino = 0;
  if (fdinfo_inode(fd, &fdinfo_ino) != 0) {
    return fail("read fdinfo inode");
  }
  if (fdinfo_ino != stat_ino) {
    fprintf(stderr, "INODE DISAGREES: fdinfo=%lu stat=%lu\n", fdinfo_ino, stat_ino);
    rc = 1;
  }

  munmap(mapping, 4096);
  close(fd);
  if (rc == 0) {
    printf("maps and stat agree: device=%x:%x inode=%lu; fdinfo inode agrees\n",
           maps_major, maps_minor, maps_ino);
  }
  return rc;
}
