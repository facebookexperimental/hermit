/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * statx_mount_id: print the mount ID that statx (STATX_MNT_ID, asked for and
 * not asked for) and the fdinfo of an open descriptor give for PATH, and
 * whether statx set the STATX_MNT_ID bit in its result mask. Linux fills
 * stx_mnt_id and sets the bit whether or not the caller asked
 * (vfs_statx_path in fs/stat.c), and fdinfo's mnt_id is the same mount's ID.
 *
 * Output, one line each:
 *   statx asked: <mnt_id> <bit>
 *   statx unasked: <mnt_id> <bit>
 *   fdinfo: <mnt_id>
 *
 * usage: statx_mount_id PATH
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char** argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: %s PATH\n", argv[0]);
    return 2;
  }
  unsigned int masks[] = {STATX_BASIC_STATS | STATX_MNT_ID, STATX_BASIC_STATS};
  const char* names[] = {"asked", "unasked"};
  for (int i = 0; i < 2; i++) {
    struct statx extended;
    memset(&extended, 0, sizeof(extended));
    if (statx(AT_FDCWD, argv[1], 0, masks[i], &extended) != 0) {
      perror("statx");
      return 1;
    }
    printf("statx %s: %llu %d\n", names[i], (unsigned long long)extended.stx_mnt_id,
           (extended.stx_mask & STATX_MNT_ID) != 0);
  }
  int fd = open(argv[1], O_RDONLY);
  if (fd < 0) {
    perror("open");
    return 1;
  }
  char path[64];
  snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
  FILE* fdinfo = fopen(path, "r");
  if (fdinfo == NULL) {
    perror("fopen fdinfo");
    return 1;
  }
  char line[256];
  while (fgets(line, sizeof(line), fdinfo) != NULL) {
    unsigned long long id;
    if (sscanf(line, "mnt_id: %llu", &id) == 1) {
      printf("fdinfo: %llu\n", id);
    }
  }
  fclose(fdinfo);
  close(fd);
  return 0;
}
