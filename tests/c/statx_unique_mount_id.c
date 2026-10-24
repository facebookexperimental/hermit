/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * statx_unique_mount_id: what statx reports for a mount ID when asked for the
 * classic one (STATX_TYPE|STATX_INO|STATX_MNT_ID, 0x1101, systemd chase()'s
 * first call) and for the unique one (0x5109, its follow-up, which adds
 * STATX_UID and STATX_MNT_ID_UNIQUE). Linux fills one mount ID either way: the
 * unique one with only STATX_MNT_ID_UNIQUE if that was asked for, otherwise
 * the classic one with STATX_MNT_ID (vfs_statx_path in fs/stat.c). Unique IDs
 * are above 2^31, the offset Linux adds so that none equals a classic one.
 *
 * usage:
 *   statx_unique_mount_id compare PATH OTHER SAME
 *       Host-independent lines, the same natively and under Hermit: for PATH,
 *       the mount bits each request returns, whether the classic ID is the
 *       descriptor's fdinfo mnt_id, whether the unique ID is above 2^31 and
 *       the same on a second call; then whether OTHER (another mount) and
 *       SAME (a path on PATH's mount) share PATH's IDs.
 *   statx_unique_mount_id raw PATH
 *       "classic=<id> unique=<id>" for PATH.
 */

#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#ifndef STATX_MNT_ID_UNIQUE
#define STATX_MNT_ID_UNIQUE 0x4000U
#endif

#define CLASSIC_REQUEST (STATX_TYPE | STATX_INO | STATX_MNT_ID)
#define UNIQUE_REQUEST (CLASSIC_REQUEST | STATX_UID | STATX_MNT_ID_UNIQUE)
#define MOUNT_BITS (STATX_MNT_ID | STATX_MNT_ID_UNIQUE)

static void query(const char* path, unsigned int mask, struct statx* result) {
  memset(result, 0, sizeof(*result));
  if (statx(AT_FDCWD, path, 0, mask, result) != 0) {
    perror("statx");
    exit(1);
  }
}

static unsigned long long fdinfo_mount_id(const char* path) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    perror("open");
    exit(1);
  }
  char name[64];
  snprintf(name, sizeof(name), "/proc/self/fdinfo/%d", fd);
  FILE* fdinfo = fopen(name, "r");
  if (fdinfo == NULL) {
    perror("fopen fdinfo");
    exit(1);
  }
  char line[256];
  unsigned long long id = 0;
  while (fgets(line, sizeof(line), fdinfo) != NULL) {
    if (sscanf(line, "mnt_id: %llu", &id) == 1) {
      break;
    }
  }
  fclose(fdinfo);
  close(fd);
  return id;
}

static int compare(const char* path, const char* other, const char* same) {
  struct statx classic, unique, again, other_classic, other_unique, same_classic, same_unique;
  query(path, CLASSIC_REQUEST, &classic);
  query(path, UNIQUE_REQUEST, &unique);
  query(path, UNIQUE_REQUEST, &again);
  query(other, CLASSIC_REQUEST, &other_classic);
  query(other, UNIQUE_REQUEST, &other_unique);
  query(same, CLASSIC_REQUEST, &same_classic);
  query(same, UNIQUE_REQUEST, &same_unique);
  printf("requested=0x%x mount_bits=0x%x fdinfo_match=%d\n", CLASSIC_REQUEST,
         classic.stx_mask & MOUNT_BITS, classic.stx_mnt_id == fdinfo_mount_id(path));
  printf("requested=0x%x mount_bits=0x%x above_offset=%d stable=%d\n", UNIQUE_REQUEST,
         unique.stx_mask & MOUNT_BITS, unique.stx_mnt_id > (1ULL << 31),
         unique.stx_mnt_id == again.stx_mnt_id);
  printf("other mount: classic_differs=%d unique_differs=%d\n",
         other_classic.stx_mnt_id != classic.stx_mnt_id,
         other_unique.stx_mnt_id != unique.stx_mnt_id);
  printf("same mount: classic_equal=%d unique_equal=%d\n",
         same_classic.stx_mnt_id == classic.stx_mnt_id,
         same_unique.stx_mnt_id == unique.stx_mnt_id);
  return 0;
}

int main(int argc, char** argv) {
  if (argc == 5 && strcmp(argv[1], "compare") == 0) {
    return compare(argv[2], argv[3], argv[4]);
  }
  if (argc == 3 && strcmp(argv[1], "raw") == 0) {
    struct statx classic, unique;
    query(argv[2], CLASSIC_REQUEST, &classic);
    query(argv[2], UNIQUE_REQUEST, &unique);
    printf("classic=%llu unique=%llu\n", (unsigned long long)classic.stx_mnt_id,
           (unsigned long long)unique.stx_mnt_id);
    return 0;
  }
  fprintf(stderr, "usage: %s compare PATH OTHER SAME | raw PATH\n", argv[0]);
  return 2;
}
