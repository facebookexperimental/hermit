/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Contract: timestamps on files the GUEST creates are deterministic.
 *
 * This surface looks covered and is not. `backend-parity-c/statx-metadata` and
 * `backend-parity-c/utimensat-determinism` both exist, and both are ci=false in
 * all five modes, so neither has ever run. 84 of the 85 cells in that bucket are
 * in the same state. A coverage check by cell NAME says this is guarded; a
 * coverage check by ci=true mode says it is not.
 *
 * Why it matters: every file a guest writes gets an mtime, and that mtime comes
 * from the clock at write time. If the clock is virtualised but the filesystem
 * timestamp is not, a build, an archive, a cache key, or anything that hashes
 * file metadata becomes nondeterministic while every read and write still
 * succeeds. It is the same shape as the /proc leak -- nothing errors, the values
 * are simply the host's.
 *
 * WHAT IS ASSERTED, AND WHAT IS NOT.
 *
 * Timestamps are printed, never compared against constants. They MUST keep
 * advancing: a file written later in the run should be able to have a later
 * mtime than one written earlier, and freezing every timestamp to a fixed value
 * would satisfy an equality check while destroying the ordering information that
 * makes mtimes useful (#140 again -- a frozen clock is not determinism). What is
 * required is that the same sequence of writes produces the same sequence of
 * timestamps every run.
 *
 * ORDERING is therefore checked explicitly, as a derived relation rather than as
 * a raw value: `later >= earlier` is printed as its own line. A run where the
 * absolute values changed but the ordering held would still be a divergence and
 * still be caught by the raw values; a run where the ORDERING inverted is a
 * different and worse bug, and printing the relation names it directly instead
 * of leaving a reader to compare two hex numbers.
 *
 * All paths are relative, so they land in the guest's own working directory and
 * the fixture never depends on host filesystem state.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

/* Print one timestamp. Seconds and nanoseconds separately: a tool can easily
 * determinize the second while leaving the nanosecond host-derived, and a
 * combined print would hide exactly that. */
static void show_time(const char* label, const struct timespec* t) {
  printf("  %-10s %lld.%09ld\n", label, (long long)t->tv_sec, (long)t->tv_nsec);
}

static void show_stat(const char* path) {
  struct stat st;
  if (stat(path, &st) != 0) {
    printf("STAT %s UNREADABLE %s\n", path, strerror(errno));
    return;
  }
  printf("STAT %s\n", path);
  /* size and mode are included because a timestamp fixture that ignored them
   * would miss a determinization that got the times right and the metadata
   * wrong. Inode and device are deliberately printed too -- a host inode
   * reaching the guest is a known leak class. */
  printf("  %-10s %lld\n", "size", (long long)st.st_size);
  printf("  %-10s 0%o\n", "mode", (unsigned)(st.st_mode & 07777));
  printf("  %-10s %lld\n", "nlink", (long long)st.st_nlink);
  printf("  %-10s %lld\n", "ino", (long long)st.st_ino);
  printf("  %-10s %lld\n", "dev", (long long)st.st_dev);
  printf("  %-10s %lld\n", "uid", (long long)st.st_uid);
  printf("  %-10s %lld\n", "gid", (long long)st.st_gid);
  show_time("atime", &st.st_atim);
  show_time("mtime", &st.st_mtim);
  show_time("ctime", &st.st_ctim);
}

static int write_file(const char* path, const char* contents) {
  int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd < 0) {
    return -1;
  }
  ssize_t n = write(fd, contents, strlen(contents));
  close(fd);
  return n < 0 ? -1 : 0;
}

static int mtime_le(const char* a, const char* b) {
  struct stat sa, sb;
  if (stat(a, &sa) != 0 || stat(b, &sb) != 0) {
    return -1;
  }
  if (sa.st_mtim.tv_sec != sb.st_mtim.tv_sec) {
    return sa.st_mtim.tv_sec <= sb.st_mtim.tv_sec;
  }
  return sa.st_mtim.tv_nsec <= sb.st_mtim.tv_nsec;
}

int main(void) {
  /* --- create, in a known order ------------------------------------------ */
  if (write_file("ts_first.txt", "first\n") != 0) {
    printf("SETUP write ts_first.txt FAILED %s\n", strerror(errno));
    return 1;
  }
  if (write_file("ts_second.txt", "second, and longer\n") != 0) {
    printf("SETUP write ts_second.txt FAILED %s\n", strerror(errno));
    return 1;
  }
  if (mkdir("ts_dir", 0755) != 0 && errno != EEXIST) {
    printf("SETUP mkdir ts_dir FAILED %s\n", strerror(errno));
    return 1;
  }
  if (write_file("ts_dir/inner.txt", "inner\n") != 0) {
    printf("SETUP write ts_dir/inner.txt FAILED %s\n", strerror(errno));
    return 1;
  }

  show_stat("ts_first.txt");
  show_stat("ts_second.txt");
  show_stat("ts_dir");
  show_stat("ts_dir/inner.txt");

  /* --- ORDERING, as a derived relation ------------------------------------
   * Printed as its own line so an inverted ordering is named directly rather
   * than left for a reader to infer by comparing two timestamps. */
  printf("ORDER first_le_second %d\n", mtime_le("ts_first.txt", "ts_second.txt"));
  printf("ORDER second_le_inner %d\n", mtime_le("ts_second.txt", "ts_dir/inner.txt"));

  /* --- rewrite: does mtime advance, and reproducibly? --------------------- */
  if (write_file("ts_first.txt", "first, rewritten\n") == 0) {
    printf("AFTER REWRITE\n");
    show_stat("ts_first.txt");
    printf("ORDER rewritten_ge_second %d\n", mtime_le("ts_second.txt", "ts_first.txt"));
  }

  /* --- explicit timestamp control -----------------------------------------
   * utimensat with a fixed value must land exactly; UTIME_NOW must be
   * reproducible. The first is a correctness check the guest can rely on, the
   * second is the determinism question. */
  struct timespec fixed[2] = {
      {.tv_sec = 1000000, .tv_nsec = 123456789},
      {.tv_sec = 2000000, .tv_nsec = 987654321},
  };
  if (utimensat(AT_FDCWD, "ts_second.txt", fixed, 0) == 0) {
    printf("AFTER UTIMENSAT FIXED\n");
    show_stat("ts_second.txt");
  } else {
    printf("utimensat fixed FAILED %s\n", strerror(errno));
  }

  struct timespec now[2] = {
      {.tv_sec = 0, .tv_nsec = UTIME_NOW},
      {.tv_sec = 0, .tv_nsec = UTIME_NOW},
  };
  if (utimensat(AT_FDCWD, "ts_second.txt", now, 0) == 0) {
    printf("AFTER UTIMENSAT NOW\n");
    show_stat("ts_second.txt");
  } else {
    printf("utimensat now FAILED %s\n", strerror(errno));
  }

  /* --- statx, for the fields plain stat cannot show -----------------------
   * btime in particular: a creation time is set once and never updated, so a
   * tool that determinizes mtime on write can still leak the host clock here. */
  struct statx sx;
  if (statx(AT_FDCWD, "ts_first.txt", 0, STATX_ALL, &sx) == 0) {
    printf("STATX ts_first.txt mask=0x%x\n", sx.stx_mask);
    printf("  btime      %lld.%09u\n", (long long)sx.stx_btime.tv_sec, sx.stx_btime.tv_nsec);
    printf("  mtime      %lld.%09u\n", (long long)sx.stx_mtime.tv_sec, sx.stx_mtime.tv_nsec);
    printf("  blocks     %lld\n", (long long)sx.stx_blocks);
    printf("  blksize    %u\n", sx.stx_blksize);
    printf("  attributes 0x%llx\n", (unsigned long long)sx.stx_attributes);
  } else {
    printf("STATX ts_first.txt FAILED %s\n", strerror(errno));
  }

  /* Clean up so a rerun in the same directory starts from the same state; a
   * fixture that left files behind would compare differently on the second
   * invocation and read as a determinism failure. */
  unlink("ts_first.txt");
  unlink("ts_second.txt");
  unlink("ts_dir/inner.txt");
  rmdir("ts_dir");
  return 0;
}
